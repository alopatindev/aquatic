#!/bin/sh 

. ./scripts/env-native-cpu-without-avx-512

cargo run --profile "release-debug" -p aquatic_udp_load_test -- $@
