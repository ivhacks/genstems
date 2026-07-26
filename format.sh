#!/bin/sh
cargo fmt
cargo clippy --fix --allow-dirty --allow-staged 2>/dev/null