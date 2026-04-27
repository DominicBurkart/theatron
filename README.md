# Theatron

[![CI](https://github.com/DominicBurkart/theatron/workflows/CI/badge.svg)](https://github.com/DominicBurkart/theatron/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/DominicBurkart/theatron/branch/main/graph/badge.svg)](https://codecov.io/gh/DominicBurkart/theatron)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![last commit](https://img.shields.io/github/last-commit/dominicburkart/theatron)](https://github.com/DominicBurkart/theatron)

Simulation framework for evaluating and comparing wireless protocol implementations under network-level effects (propagation, interference, contention, adversarial scenarios). LoRaWAN via lora-rs is the first validation target.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md) — design, core abstractions, phased roadmap.

## Examples

- [`examples/aloha`](examples/aloha) — Pure ALOHA reference protocol.
- [`examples/lorawan_file_transfer`](examples/lorawan_file_transfer) — LoRaWAN Class A via `lorawan-device`, with a simulated radio and network server.

Run an example:

```sh
cargo run --example aloha
cargo run --example lorawan_file_transfer
```

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
