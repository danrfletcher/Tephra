#!/bin/bash
# helpers
greet() {
  echo "hi"
  inner() { :; }
}

function build {
  echo build
}

function test_all() {
  echo test
}

VERSION=1
