#! /usr/bin/env nix-shell
#! nix-shell -i bash -p pandoc ronn

set -eux

target="${1:-default}"
dir="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"

cd "$dir"

case "$target" in
  default)
    echo "Try ./do test , ./do doc or ./do format-doc."
  ;;
  format-doc)
    pandoc -f gfm -t gfm man/orderly.1.md > man/orderly.1.md.tmp
    mv man/orderly.1.md.tmp man/orderly.1.md
  ;;
  doc)
    rm -rf ./man/generated
    mkdir -p ./man/generated
    cp man/orderly.1.md ./man/generated/
    cd ./man/generated
    ronn orderly.1.md
    rm orderly.1.md
    MANWIDTH=100 man -l ./orderly.1 | col -bx > ./orderly.1.txt
  ;;
  test)
    cargo build
    export PATH="$PATH:$(pwd)/target/debug/"
    ./test/run_tests
  ;;
  *)
    echo "Don't know how to do '$target'"
    exit 1
  ;;
esac