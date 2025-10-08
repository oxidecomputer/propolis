#!/usr/bin/env bash

# This file is adapted from Omicron's install_builder_prerequisites.sh.

set -eu

MARKER=/etc/opt/oxide/NO_INSTALL
if [[ -f "$MARKER" ]]; then
  echo "This system has the marker file $MARKER, aborting." >&2
  exit 1
fi

# Set the CWD to Omicron's source.
SOURCE_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )"
cd "${SOURCE_DIR}/.."

function on_exit
{
  echo "Something went wrong, but this script is idempotent - If you can fix the issue, try re-running"
}

trap on_exit ERR

function usage
{
  echo "Usage: ./install_builder_prerequisites.sh <OPTIONS>"
  echo "  Options: "
  echo "   -y: Assume 'yes' instead of showing confirmation prompts"
  echo "   -p: Skip checking paths"
  echo "   -r: Number of retries to perform for network operations (default: 3)"
  exit 1
}

ASSUME_YES="false"
SKIP_PATH_CHECK="false"
OMIT_SUDO="false"
RETRY_ATTEMPTS=3
while getopts ypsr: flag
do
  case "${flag}" in
    y) ASSUME_YES="true" ;;
    p) SKIP_PATH_CHECK="true" ;;
    s) OMIT_SUDO="true" ;;
    r) RETRY_ATTEMPTS=${OPTARG} ;;
    *) usage
  esac
done

# Offers a confirmation prompt, unless we were passed `-y`.
#
# Args:
#  $1: Text to be displayed
function confirm
{
  if [[ "${ASSUME_YES}" == "true" ]]; then
    response=y
  else
    read -r -p "$1 (y/n): " response
  fi
  case $response in
    [yY])
      true
      ;;
    *)
      false
      ;;
  esac
}

# Function which executes all provided arguments, up to ${RETRY_ATTEMPTS}
# times, or until the command succeeds.
function retry
{
  attempts="${RETRY_ATTEMPTS}"
  # Always try at least once
  attempts=$((attempts < 1 ? 1 : attempts))
  for i in $(seq 1 $attempts); do
    retry_rc=0
    "$@" || retry_rc=$?;
    if [[ "$retry_rc" -eq 0 ]]; then
      return
    fi

    if [[ $i -ne $attempts ]]; then
      echo "Failed to run command -- will try $((attempts - i)) more times"
    fi
  done

  exit $retry_rc
}

function xtask
{
  if [ -z ${XTASK_BIN+x} ]; then
    cargo xtask "$@"
  else
    "$XTASK_BIN" "$@"
  fi
}

HOST_OS=$(uname -s)

function install_packages {
  if [[ "${HOST_OS}" == "Linux" ]]; then
    # TODO: configure a Nix flake for folks who prefer that

    packages=(
      'build-essential'
      'libclang-dev'
      'pkg-config'
    )
    if [[ "${OMIT_SUDO}" == "false" ]]; then
      maybe_sudo="sudo"
    else
      maybe_sudo=""
    fi
    $maybe_sudo apt-get update
    if [[ "${ASSUME_YES}" == "true" ]]; then
        $maybe_sudo apt-get install -y "${packages[@]}"
    else
        confirm "Install (or update) [${packages[*]}]?" && $maybe_sudo apt-get install "${packages[@]}"
    fi
  elif [[ "${HOST_OS}" == "SunOS" ]]; then
    CLANGVER=15
    PGVER=13
    packages=(
      "pkg:/package/pkg"
      "build-essential"
      "pkg-config"
      # "bindgen leverages libclang to preprocess, parse, and type check C and C++ header files."
      "pkg:/ooce/developer/clang-$CLANGVER"
    )

    # Install/update the set of packages.
    # Explicitly manage the return code using "rc" to observe the result of this
    # command without exiting the script entirely (due to bash's "errexit").
    rc=0
    confirm "Install (or update) [${packages[*]}]?" && { pfexec pkg install -v "${packages[@]}" || rc=$?; }
    # Return codes:
    #  0: Normal Success
    #  4: Failure because we're already up-to-date. Also acceptable.
    if ((rc != 4 && rc != 0)); then
      exit "$rc"
    fi

    confirm "Set mediators?" && {
      pfexec pkg set-mediator -V $CLANGVER clang llvm
    }

    pkg mediator -a
    pkg publisher
    pkg list -afv "${packages[@]}"
  elif [[ "${HOST_OS}" == "Darwin" ]]; then
    # clang is expected to be installed via the Xcode Command Line Tools.
    echo "Nothing to do on macOS"
  else
    echo "Unsupported OS: ${HOST_OS}"
    exit 1
  fi
}

retry install_packages

# Validate the PATH:
expected_in_path=(
  'pkg-config'
)

function show_hint
{
  case "$1" in
    "pkg-config")
      if [[ "${HOST_OS}" == "SunOS" ]]; then
        echo "On illumos, $1 is typically found in '/usr/bin'"
      fi
      ;;
    *)
      ;;
  esac
}

# Check all paths before returning an error, unless we were told not too.
if [[ "$SKIP_PATH_CHECK" == "true" ]]; then
  echo "All prerequisites installed successfully"
  exit 0
fi

ANY_PATH_ERROR="false"
for command in "${expected_in_path[@]}"; do
  rc=0
  which "$command" &> /dev/null || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    echo "ERROR: $command seems installed, but was not found in PATH. Please add it."
    show_hint "$command"
    ANY_PATH_ERROR="true"
  fi
done

if [[ "$ANY_PATH_ERROR" == "true" ]]; then
  exit 1
fi

echo "All builder prerequisites installed successfully, and PATH looks valid"
