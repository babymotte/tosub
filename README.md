# TOSUB

Subsystems for [tokio](https://tokio.rs/)

## Motivation

This library is heavily inspired by and solves the same problem as [tokio-graceful-shutdown](https://github.com/Finomnis/tokio-graceful-shutdown), however the author of `tokio-graceful-shutdown` made a few choices that made it tricky to use in some of my use cases, so I decided to write my own version of it.

The major difference is that in `tosub` instances of `Subsystem` (the equivalent to `tokio-graceful-shutdown`'s `SubsystemHandle`) are owned and implement `Clone`, which makes it easier to pass them around.

## Compatibility

The pre-release version of `tosub` is unix-only since I have not yet implemented platform independent signal handaling, however I'm planning to make it fully cross-platform for `1.0`.
