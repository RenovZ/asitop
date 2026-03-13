# asitop

Performance monitoring CLI tool for Apple Silicon

![](images/asitop.png)

```shell
cargo run -- --help
```

## What is `asitop`

A Rust-based `nvtop`-inspired command line tool for Apple Silicon Macs.

* Utilization info:
  * CPU (E-cluster and P-cluster), GPU
  * Frequency and utilization
  * ANE utilization (measured by power)
* Memory info:
  * RAM and swap, size and usage
* Power info:
  * CPU power, GPU power
  * Rolling power history
  * Peak power and averaged power display

`asitop` uses the built-in [`powermetrics`](https://www.unix.com/man-page/osx/1/powermetrics/) utility on macOS, which allows access to hardware performance counters. It still requires `sudo`, because `powermetrics` itself needs elevated privileges.

## Installation and Usage

The active implementation in this repository is now written in Rust.

```shell
# build
cargo build

# run
cargo run

# show help
cargo run -- --help
```

```shell
# run the release build directly
./target/release/asitop --interval 1 --avg 30 --show-cores
```

Available flags:

```text
--interval <SECONDS>   Display and sampling interval
--color <0-8>          Gauge color index
--avg <SECONDS>        Averaging window for power metrics
--show-cores           Show per-core details
--max-count <N>        Restart powermetrics after N refreshes
```

Press `q` or `Esc` to quit the TUI.

## How it works

`powermetrics` is used to measure the following:

* CPU/GPU utilization via active residency
* CPU/GPU frequency
* CPU/GPU/ANE/package energy consumption

Rust reads memory and swap usage via [`sysinfo`](https://crates.io/crates/sysinfo).

[`sysctl`](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/sysctl.3.html) is used to measure the following:

* CPU name
* CPU core counts

[`system_profiler`](https://ss64.com/osx/system_profiler.html) is used to measure the following:

* GPU core count

Some information is guesstimated because macOS does not expose a single official source for all of it:

* CPU/GPU TDP
* ANE max power

## Legacy Python Version

The previous Python implementation is preserved under `legacy/` for reference.
