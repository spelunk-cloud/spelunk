#!/bin/sh

MAX_TIME=600
JOBS=4

cargo +nightly fuzz run fuzz_chunker -- -max_total_time=$MAX_TIME -jobs=$JOBS
cargo +nightly fuzz run fuzz_cli_history_entry -- -max_total_time=$MAX_TIME -jobs=$JOBS
cargo +nightly fuzz run fuzz_escape_xml -- -max_total_time=$MAX_TIME -jobs=$JOBS
cargo +nightly fuzz run fuzz_jsonl_parse -- -max_total_time=$MAX_TIME -jobs=$JOBS
cargo +nightly fuzz run fuzz_parser -- -max_total_time=$MAX_TIME -jobs=$JOBS
cargo +nightly fuzz run fuzz_secret_scanner -- -max_total_time=$MAX_TIME -jobs=$JOBS
