#!/bin/bash
set -e

echo "Checking Rust workspace..."
cargo check

echo "Running tests..."
cargo test

echo "Building dev workspace..."
cargo build
