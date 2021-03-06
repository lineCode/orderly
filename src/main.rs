mod specs;
use std::ops::Add;
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

struct RateLimiter {
  capacity: f64,
  tokens: f64,
  tokens_per_sec: f64,
  last_add: std::time::Instant,
}

struct Supervisor {
  spec: specs::SupervisorSpec,
  procs: Vec<Option<std::process::Child>>,
  rate_limiter: RateLimiter,
  sigrx: crossbeam_channel::Receiver<Signal>,
  first_start: bool,
}

enum Signal {
  Shutdown,
  Terminate,
}

#[derive(Debug)]
enum SupervisorError {
  IOError(std::io::Error),
  Shutdown,
  Terminated,
  RestartLimitReached,
  ProcFailed,
  UnkillableChild,
}

impl From<std::io::Error> for SupervisorError {
  fn from(e: std::io::Error) -> Self {
    SupervisorError::IOError(e)
  }
}

impl RateLimiter {
  pub fn new(mut capacity: f64, tokens_per_sec: f64) -> Self {
    if capacity < 1.0 {
      capacity = 1.0;
    };
    if tokens_per_sec < 0.0 {
      capacity = 0.0;
    };
    RateLimiter {
      capacity,
      tokens_per_sec,
      tokens: capacity,
      last_add: Instant::now(),
    }
  }

  pub fn take(&mut self) -> bool {
    self.add_tokens();

    if self.tokens < 1.0 {
      false
    } else {
      self.tokens -= 1.0;
      true
    }
  }

  fn add_tokens(&mut self) {
    let now = std::time::Instant::now();
    let duration = now.duration_since(self.last_add);
    let millis = 1000 * duration.as_secs() + duration.subsec_millis() as u64;
    let new_tokens = ((millis as f64) * self.tokens_per_sec) / 1000.0;
    self.tokens += new_tokens;
    if self.tokens > self.capacity {
      self.tokens = self.capacity;
    }
    self.last_add = now;
  }
}

impl Supervisor {
  fn new(spec: specs::SupervisorSpec, sigrx: crossbeam_channel::Receiver<Signal>) -> Self {
    let mut procs = vec![];
    for _i in spec.procs.iter() {
      procs.push(None);
    }

    let rate_limiter = RateLimiter::new(spec.max_restart_tokens, spec.restart_tokens_per_second);

    Supervisor {
      spec,
      procs,
      sigrx,
      rate_limiter,
      first_start: true,
    }
  }

  fn write_status_file(&mut self, status: &str) -> Result<(), SupervisorError> {
    match self.spec.status_file {
      Some(ref status_file) => {
        let status_file = std::path::PathBuf::from(status_file);
        let mut tmp_path = status_file.clone();
        let mut ext = if let Some(ext) = tmp_path.extension() {
          String::from(ext.to_str().unwrap_or(""))
        } else {
          String::from("")
        };
        ext.push_str(".tmp");
        tmp_path.set_extension(ext);

        std::fs::write(&tmp_path, status)?;
        std::fs::rename(&tmp_path, &status_file)?;
        Ok(())
      }
      None => Ok(()),
    }
  }

  fn check_signals(&mut self) -> Result<(), SupervisorError> {
    match self.sigrx.try_recv() {
      Ok(Signal::Shutdown) => return Err(SupervisorError::Shutdown),
      Ok(Signal::Terminate) => return Err(SupervisorError::Terminated),
      _ => Ok(()),
    }
  }

  fn sleep(&mut self, d: Duration) -> Result<(), SupervisorError> {
    crossbeam_channel::select! {
      recv(self.sigrx) -> sig => if let Ok(sig) = sig {
        match sig {
          Signal::Shutdown => return Err(SupervisorError::Shutdown),
          Signal::Terminate => return Err(SupervisorError::Terminated),
        }
      } else {
        return Err(SupervisorError::Terminated)
      },
      default(d) => (),
    }
    Ok(())
  }

  fn kill_child_tree(
    c: &mut std::process::Child,
    deadline: Option<Instant>,
  ) -> Result<(), SupervisorError> {
    // We busy wait here as it is simpler, if we are killing the process
    // the supervisor has work to do anyway, so it doesn't waste that much cpu.

    // First try a SIGTERM, let the process do whatever cleanup it needs to do.

    let rc = unsafe { libc::kill(-(c.id() as i32), libc::SIGTERM) };
    if rc != 0 {
      log::warn!("sending SIGTERM to process group failed.");
    }

    loop {
      if let Some(deadline) = deadline {
        if Instant::now() >= deadline {
          break;
        }
      }
      match c.try_wait() {
        Err(_) => break, /* Go straight to kill */
        Ok(None) => (),
        Ok(Some(_)) => return Ok(()),
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    log::warn!("child did not respond to SIGTERM, trying SIGKILL.");

    let rc = unsafe { libc::kill(-(c.id() as i32), libc::SIGKILL) };
    if rc != 0 {
      log::warn!("killing process group failed.");
    }

    for _ in 0..1000 {
      match c.try_wait() {
        Err(_) => (),
        Ok(None) => (),
        Ok(_) => return Ok(()),
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    Err(SupervisorError::UnkillableChild)
  }

  fn spawn_child(
    command: &str,
    env: &Vec<(String, String)>,
  ) -> Result<std::process::Child, SupervisorError> {
    let mut cmd = std::process::Command::new(command);
    cmd.stdin(std::process::Stdio::null());
    for v in env {
      cmd.env(&v.0, &v.1);
    }
    cmd.before_exec(|| {
      match nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0)) {
        Ok(_pid) => Ok(()),
        Err(_err) => Err(std::io::Error::from(std::io::ErrorKind::Other)),
      }
    });
    Ok(cmd.spawn()?)
  }

  fn deadline_from_float_seconds(start: Instant, timeout_seconds: Option<f64>) -> Option<Instant> {
    match timeout_seconds {
      Some(secs) => Some(start.add(Duration::from_millis((secs * 1000.0) as u64))),
      None => None,
    }
  }

  fn run_command_timeout_secs(
    &mut self,
    command: &str,
    env: &Vec<(String, String)>,
    timeout_secs: Option<f64>,
    depends_on_proc: Option<usize>,
  ) -> Result<(), SupervisorError> {
    self.run_command(
      command,
      env,
      Supervisor::deadline_from_float_seconds(Instant::now(), timeout_secs),
      depends_on_proc,
    )
  }

  fn run_command(
    &mut self,
    command: &str,
    env: &Vec<(String, String)>,
    deadline: Option<Instant>,
    depends_on_proc: Option<usize>,
  ) -> Result<(), SupervisorError> {
    let mut c = Supervisor::spawn_child(command, env)?;

    let max_delay: u64 = 500;
    let mut delay: u64 = 10;

    loop {
      self.check_signals()?;

      if let Some(deadline) = deadline {
        let now = Instant::now();
        if now > deadline {
          Supervisor::kill_child_tree(&mut c, Some(now.add(Duration::from_secs(10))))?;
          return Err(SupervisorError::ProcFailed);
        }
      }

      if let Some(idx) = depends_on_proc {
        let ok = match self.procs[idx] {
          Some(ref mut p) => match p.try_wait() {
            Ok(None) => true,
            _ => false,
          },
          None => false,
        };

        if !ok {
          Supervisor::kill_child_tree(
            &mut c,
            Supervisor::deadline_from_float_seconds(Instant::now(), Some(10.0)),
          )?;
          return Err(SupervisorError::ProcFailed);
        }
      }

      match c.try_wait()? {
        Some(rc) => {
          return if rc.success() {
            Ok(())
          } else {
            Err(SupervisorError::ProcFailed)
          };
        }
        None => {
          self.sleep(Duration::from_millis(delay))?;
          delay += 50;
          if delay > max_delay {
            delay = max_delay
          }
        }
      };
    }
  }

  fn get_supervisor_script_env(action: &str) -> Vec<(String, String)> {
    vec![(String::from("ORDERLY_ACTION"), String::from(action))]
  }

  fn get_proc_script_env(&mut self, action: &str, idx: usize) -> Vec<(String, String)> {
    let mut env = Supervisor::get_supervisor_script_env(action);

    env.push((
      String::from("ORDERLY_SERVICE_NAME"),
      self.spec.procs[idx].name.clone(),
    ));

    if let Some(c) = &self.procs[idx] {
      env.push((String::from("ORDERLY_RUN_PID"), format!("{}", c.id())));
    }

    env
  }

  fn kill_proc(&mut self, idx: usize) -> Result<(), SupervisorError> {
    // Kill is not affected by signals...

    let p = &mut self.procs[idx];

    match p {
      Some(c) => {
        log::info!("killing {}.", self.spec.procs[idx].name.as_str());

        Supervisor::kill_child_tree(
          c,
          Supervisor::deadline_from_float_seconds(
            Instant::now(),
            self.spec.procs[idx].terminate_timeout_seconds,
          ),
        )?;
        *p = None;
      }
      None => (),
    };

    self.clean_proc(idx)?;

    Ok(())
  }

  fn shutdown_proc(&mut self, idx: usize) -> Result<(), SupervisorError> {
    self.check_signals()?;

    log::info!("shutting down {}.", self.spec.procs[idx].name.as_str());

    let start_t = Instant::now();
    let deadline = Supervisor::deadline_from_float_seconds(
      start_t,
      self.spec.procs[idx].shutdown_timeout_seconds,
    );
    let env = self.get_proc_script_env("SHUTDOWN", idx);

    match self.spec.procs[idx].shutdown {
      Some(ref shutdown) => match self.run_command(&shutdown.clone(), &env, deadline, None) {
        Ok(c) => c,
        Err(err) => {
          log::warn!("shutdown script error: {:?}.", err);
          return self.kill_proc(idx);
        }
      },
      None => return self.kill_proc(idx),
    };

    // Some duplication from run_command, but ownership makes this hard to reuse.
    let max_delay: u64 = 500;
    let mut delay: u64 = 10;

    loop {
      self.check_signals()?;

      if let Some(deadline) = deadline {
        if Instant::now() > deadline {
          log::warn!("shutdown script exited, but shutdown timed out, using kill instead.");
          return self.kill_proc(idx);
        }
      }

      {
        let p = &mut self.procs[idx];
        match p {
          Some(c) => match c.try_wait()? {
            Some(_) => {
              *p = None;
              break;
            }
            None => (),
          },
          None => break,
        };
      }

      self.sleep(Duration::from_millis(delay))?;
      delay += 50;
      if delay > max_delay {
        delay = max_delay
      }
    }

    self.clean_proc(idx)?;

    Ok(())
  }

  fn check_proc(&mut self, idx: usize) -> Result<(), SupervisorError> {
    self.check_signals()?;

    log::info!("checking {}.", self.spec.procs[idx].name);

    let env = self.get_proc_script_env("CHECK", idx);
    let p = &mut self.procs[idx];

    match p {
      Some(c) => match c.try_wait()? {
        None => {
          let s = &self.spec.procs[idx];
          match s.check {
            Some(ref check) => {
              self.run_command_timeout_secs(&check.clone(), &env, s.check_timeout_seconds, None)
            }
            None => Ok(()),
          }
        }
        Some(_) => {
          *p = None;
          Err(SupervisorError::ProcFailed)
        }
      },
      None => Err(SupervisorError::ProcFailed),
    }
  }

  fn clean_proc(&mut self, idx: usize) -> Result<(), SupervisorError> {
    self.check_signals()?;

    log::info!("running {} cleanup.", self.spec.procs[idx].name);
    if let Some(_) = self.procs[idx] {
      panic!("bug, clean without kill.")
    };

    let env = self.get_proc_script_env("CLEANUP", idx);
    let s = &self.spec.procs[idx];
    match s.cleanup {
      Some(ref cleanup) => {
        self.run_command_timeout_secs(&cleanup.clone(), &env, s.cleanup_timeout_seconds, None)
      }
      None => Ok(()),
    }
  }

  fn start_proc(&mut self, idx: usize) -> Result<(), SupervisorError> {
    self.check_signals()?;

    log::info!("starting {}.", self.spec.procs[idx].name);

    let env = self.get_proc_script_env("RUN", idx);
    let s = self.spec.procs.get(idx).unwrap();
    let c = Supervisor::spawn_child(&s.run, &env)?;
    self.procs[idx] = Some(c);

    {
      let env = self.get_proc_script_env("WAIT_STARTED", idx);
      let s = &self.spec.procs[idx];
      match s.wait_started {
        Some(ref wait_started) => self.run_command_timeout_secs(
          &wait_started.clone(),
          &env,
          s.wait_started_timeout_seconds,
          Some(idx),
        )?,
        None => (),
      }
    }

    Ok(())
  }

  fn kill_all_procs(&mut self) -> Result<(), SupervisorError> {
    for i in (0..self.procs.len()).rev() {
      self.kill_proc(i)?;
    }
    Ok(())
  }

  fn kill_all_procs_ignore_errors(&mut self) {
    for i in (0..self.procs.len()).rev() {
      if let Err(e) = self.kill_proc(i) {
        log::warn!("error while killing proc: {:?}.", e);
      }
    }
  }

  fn shutdown_all_procs(&mut self) -> Result<(), SupervisorError> {
    for i in (0..self.procs.len()).rev() {
      self.shutdown_proc(i)?;
    }
    Ok(())
  }

  fn restart_all_procs(&mut self) -> Result<(), SupervisorError> {
    log::info!("(re)starting all procs.");

    self.kill_all_procs()?;

    for i in 0..self.procs.len() {
      self.start_proc(i)?;
    }

    Ok(())
  }

  fn check_all_procs(&mut self) -> Result<(), SupervisorError> {
    for i in 0..self.procs.len() {
      self.check_proc(i)?;
    }

    Ok(())
  }

  fn supervise(&mut self, num_restarts: u128) -> SupervisorError {
    if self.first_start {
      if let Err(e) = self.write_status_file("STARTING\n") {
        return e;
      }
    }

    if !self.rate_limiter.take() {
      return SupervisorError::RestartLimitReached;
    }

    if num_restarts > 0 {
      if let Some(ref restart) = self.spec.restart {
        if let Err(e) = self.run_command(
          &restart.clone(),
          &Supervisor::get_supervisor_script_env("RESTART"),
          Supervisor::deadline_from_float_seconds(Instant::now(), self.spec.failure_timeout),
          None,
        ) {
          log::error!("error running restart lifecycle hook: {:?}.", e);
        }
      }
    }

    match self.restart_all_procs() {
      Ok(()) => (),
      Err(e) => return e,
    };

    if self.first_start {
      self.first_start = false;

      if let Err(e) = self.write_status_file("RUNNING\n") {
        return e;
      }

      if let Some(ref start_complete) = self.spec.start_complete {
        if let Err(e) = self.run_command(
          &start_complete.clone(),
          &Supervisor::get_supervisor_script_env("START_COMPLETE"),
          Supervisor::deadline_from_float_seconds(Instant::now(), self.spec.start_complete_timeout),
          None,
        ) {
          return e;
        }
      }
    }

    loop {
      match self.check_all_procs() {
        Ok(()) => match self.sleep(Duration::from_millis(
          (self.spec.check_delay_seconds * 1000.0) as u64,
        )) {
          Ok(()) => continue,
          Err(e) => return e,
        },
        Err(e) => return e,
      }
    }
  }

  fn supervise_forever(&mut self) {
    let rc: i32;

    let mut num_restarts: u128 = 0;

    loop {
      match self.supervise(num_restarts) {
        e @ SupervisorError::IOError(_) | e @ SupervisorError::ProcFailed => {
          num_restarts = num_restarts + 1;
          log::warn!(
            "supervisor encountered an error: {:?} (restarts={}).",
            e,
            num_restarts
          );
        }
        SupervisorError::Shutdown => {
          log::info!("supervisor shutting down gracefully.");
          match self.shutdown_all_procs() {
            Ok(()) => (),
            Err(e) => {
              log::error!("unable shutdown child procs, killing instead: {:?}.", e);
              self.kill_all_procs_ignore_errors();
            }
          }
          rc = 0;
          break;
        }
        e @ SupervisorError::Terminated
        | e @ SupervisorError::RestartLimitReached
        | e @ SupervisorError::UnkillableChild => {
          log::error!(
            "supervisor unable to continue: {:?} - shutting down brutally.",
            e
          );
          self.kill_all_procs_ignore_errors();

          if let Some(ref failure) = self.spec.failure {
            if let Err(e) = self.run_command(
              &failure.clone(),
              &Supervisor::get_supervisor_script_env("FAILURE"),
              Supervisor::deadline_from_float_seconds(Instant::now(), self.spec.failure_timeout),
              None,
            ) {
              log::error!("error running failure lifecycle hook: {:?}.", e);
            }
          }

          rc = 1;
          break;
        }
      }
    }

    if let Some(ref path) = self.spec.status_file {
      if let Err(err) = std::fs::remove_file(path) {
        log::warn!("error removing status file: {}.", err);
      }
    }

    std::process::exit(rc);
  }
}

fn usage() {
  println!("{}", include_str!("../man/generated/orderly.1.txt"));
  std::process::exit(0);
}

fn version() {
  println!("{} - {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
  std::process::exit(0);
}

fn die(s: &str) -> ! {
  log::error!("{}", s);
  std::process::exit(1);
}

fn main() {
  simple_logger::init().unwrap();

  let args: Vec<String> = std::env::args().collect();
  let mut arg_idx = 1;

  let mut supervisor_spec_builder = specs::SupervisorSpecBuilder::new();
  let mut proc_spec_builder = specs::ProcSpecBuilder::new();

  for a in &args {
    if a == "--" {
      break;
    }

    if a == "-h" || a == "-help" || a == "--help" {
      usage();
    }

    if a == "-version" || a == "--version" {
      version();
    }
  }

  macro_rules! float_arg {
    () => {{
      let arg = args
        .get(arg_idx + 1)
        .unwrap_or_else(|| die(format!("{} expects a number.", args[arg_idx]).as_ref()));

      let arg = arg
        .parse::<f64>()
        .unwrap_or_else(|_e| die(format!("{} is not a valid f64.", arg).as_ref()));

      arg_idx += 2;

      arg
    }};
  }

  macro_rules! string_arg {
    () => {{
      let arg = args
        .get(arg_idx + 1)
        .unwrap_or_else(|| die(format!("{} expected an argument.", args[arg_idx]).as_ref()));
      arg_idx += 2;
      arg.clone()
    }};
  }

  while arg_idx < args.len() {
    match args[arg_idx].as_ref() {
      "-restart-tokens-per-second" => {
        supervisor_spec_builder.set_restart_tokens_per_second(float_arg!());
      }
      "-check-delay" => {
        supervisor_spec_builder.set_check_delay_seconds(float_arg!());
      }
      "-max-restart-tokens" => {
        supervisor_spec_builder.set_max_restart_tokens(float_arg!());
      }
      "-status-file" => {
        supervisor_spec_builder.set_status_file(string_arg!());
      }
      "-start-complete" => {
        supervisor_spec_builder.set_start_complete(string_arg!());
      }
      "-start-complete-timeout" => {
        supervisor_spec_builder.set_start_complete_timeout(float_arg!());
      }
      "-on-restart" => {
        supervisor_spec_builder.set_restart(string_arg!());
      }
      "-on-restart-timeout" => {
        supervisor_spec_builder.set_restart_timeout(float_arg!());
      }
      "-on-failure" => {
        supervisor_spec_builder.set_failure(string_arg!());
      }
      "-on-failure-timeout" => {
        supervisor_spec_builder.set_failure_timeout(float_arg!());
      }
      "-all-commands" => {
        let all = args
          .get(arg_idx + 1)
          .unwrap_or_else(|| die("-all-commands expected an argument."));
        supervisor_spec_builder.set_start_complete(all.clone());
        supervisor_spec_builder.set_restart(all.clone());
        supervisor_spec_builder.set_failure(all.clone());
        arg_idx += 2;
      }
      "--" => {
        arg_idx += 1;
        break;
      }
      unknown => die(format!("unknown argument: {}.", unknown).as_ref()),
    }
  }

  while arg_idx < args.len() {
    match args[arg_idx].as_ref() {
      "-name" => {
        proc_spec_builder.set_name(string_arg!());
      }
      "-run" => {
        proc_spec_builder.set_run(string_arg!());
      }
      "-check" => {
        proc_spec_builder.set_check(string_arg!());
      }
      "-check-timeout" => {
        proc_spec_builder.set_check_timeout_seconds(float_arg!());
      }
      "-wait-started" => {
        proc_spec_builder.set_wait_started(string_arg!());
      }
      "-wait-started-timeout" => {
        proc_spec_builder.set_wait_started_timeout_seconds(float_arg!());
      }
      "-cleanup" => {
        proc_spec_builder.set_cleanup(string_arg!());
      }
      "-cleanup-timeout" => {
        proc_spec_builder.set_cleanup_timeout_seconds(float_arg!());
      }
      "-shutdown" => {
        proc_spec_builder.set_shutdown(string_arg!());
      }
      "-shutdown-timeout" => {
        proc_spec_builder.set_shutdown_timeout_seconds(float_arg!());
      }
      "-terminate-timeout" => {
        proc_spec_builder.set_terminate_timeout_seconds(float_arg!());
      }
      "-all-commands" => {
        let all = args
          .get(arg_idx + 1)
          .unwrap_or_else(|| die("-all-commands expected an argument."));
        proc_spec_builder.set_run(all.clone());
        proc_spec_builder.set_check(all.clone());
        proc_spec_builder.set_wait_started(all.clone());
        proc_spec_builder.set_shutdown(all.clone());
        proc_spec_builder.set_cleanup(all.clone());
        arg_idx += 2;
      }
      "--" => {
        match proc_spec_builder.build() {
          Ok(spec) => {
            supervisor_spec_builder.add_proc_spec(spec);
            proc_spec_builder = specs::ProcSpecBuilder::new();
          }
          Err(specs::SpecError::MissingField(f)) => {
            die(format!("proc spec missing field '{}'", f).as_ref())
          }
        }
        arg_idx += 1;
      }

      unknown => die(format!("unknown process spec argument: {}.", unknown).as_ref()),
    }
  }

  match proc_spec_builder.build() {
    Ok(spec) => supervisor_spec_builder.add_proc_spec(spec),
    Err(specs::SpecError::MissingField(f)) => {
      die(format!("proc spec missing field '{}'", f).as_ref())
    }
  };

  let spec = match supervisor_spec_builder.build() {
    Ok(spec) => spec,
    Err(specs::SpecError::MissingField(f)) => {
      die(format!("supervisor spec missing field '{}'", f).as_ref())
    }
  };

  let (sigtx, sigrx) = crossbeam_channel::bounded::<Signal>(64);

  let _ = std::thread::spawn(move || {
    if let Ok(signals) =
      signal_hook::iterator::Signals::new(&[signal_hook::SIGINT, signal_hook::SIGTERM])
    {
      for signal in signals.forever() {
        match signal {
          signal_hook::SIGINT => {
            let _ = sigtx.send(Signal::Shutdown);
          }
          signal_hook::SIGTERM => {
            let _ = sigtx.send(Signal::Terminate);
          }
          _ => (),
        }
      }
    }
  });

  if std::process::id() == 1 {
    die(format!("running as pid 1 is not supported.").as_ref());
  }

  let mut supervisor = Supervisor::new(spec, sigrx);
  supervisor.supervise_forever();
}
