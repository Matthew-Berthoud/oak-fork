#!/bin/bash
#
# Build configuration for key_xor_test_app.
#
export PACKAGE_NAME=key_xor_test_app

export BUILD_COMMAND=(
  nix
  develop
  .#rust
  --command
  env
  --chdir=enclave_apps/key_xor_test_app
  cargo
  build
  --release
)

export SUBJECT_PATHS=(
  artifacts/enclave_apps/key_xor_test_app
)
