#!/bin/bash

PHD_QUICKSTART_DIR=/tmp/phd-quickstart
if [ -f "$PHD_QUICKSTART_DIR" ]; then
	echo "$PHD_QUICKSTART_DIR exists and is not a directory"
	exit 1
fi

if [ ! -d "$PHD_QUICKSTART_DIR" ]; then
	mkdir $PHD_QUICKSTART_DIR
fi

cargo run --profile=phd --bin=phd-runner -- \
	--artifact-toml-path ./sample_artifacts.toml \
	--tmp-directory $PHD_QUICKSTART_DIR \
	--propolis-server-cmd $1
