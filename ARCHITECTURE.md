# theatron — Architecture Proposal

## Project Goal

theatron is a simulation framework for evaluating, comparing, and designing MAC-level and above LoRa protocols under adversarial conditions. The aim is to enable rigorous, reproducible protocol research: implement a protocol once against a common trait interface, run it through a shared simulation engine, and get comparable metrics automatically.

## Threat Model

The adversary space we target includes:

- **Band saturation**: flooding the channel with transmissions to prevent legitimate nodes from accessing it
- **Replay attacks**: capturing and re-transmitting valid frames to cause duplication, desynchronization, or authentication bypass
- **Jamming**: continuous or selective RF interference to disrupt reception
- **Eavesdropping**: passive traffic analysis and payload capture

Adversaries may be external (outside the protocol stack) or internal (compromised nodes participating in the network).

## Core Abstractions

### `Protocol` trait

The central abstraction. Each MAC protocol implements this trait, which defines how a node processes received frames, generates transmissions, and manages state. The trait must be object-safe to allow runtime composition and comparison.

```rust
trait Protocol {
    type State;
    type Frame;

    fn init(&self) -> Self::State;
    fn on_receive(&self, state: &mut Self::State, frame: Self::Frame, time: SimTime);
    fn poll_transmit(&self, state: &mut Self::State, time: SimTime) -> Option<Self::Frame>;
}
```

### Channel / Medium

A shared simulation object that models the physical LoRa channel: propagation delay, collision detection, RSSI, SNR, spreading factor interactions. Protocols do not talk to each other directly — all communication flows through the channel. This enforces realistic constraints and enables adversary injection.

### Adversary models

Adversaries are first-class simulation participants. They observe the channel (subject to the same physical constraints as real nodes) and may inject frames. Composition: multiple adversaries can run simultaneously. Each adversary implements an `Adversary` trait with hooks into the channel's event stream.

### Metrics collection

A passive observer attached to the simulation that records per-protocol, per-run statistics: throughput, packet delivery ratio, latency distribution, energy proxy (time-on-air), and protocol-specific counters. Output in a structured format suitable for statistical comparison across runs.

## Phased Roadmap

### Phase 1 — Core simulation engine

- Discrete-event time model (`SimTime` as a monotonic tick)
- Channel model: collision detection, spreading factor orthogonality approximation, propagation delay
- Node abstraction: schedulable agents that interact only through the channel
- Deterministic seeding for reproducibility

### Phase 2 — Protocol abstraction layer

- Finalize the `Protocol` trait API
- Frame type system (headers, payloads, metadata)
- Node lifecycle management (join, active, sleep, leave)
- Hook points for metrics collection

### Phase 3 — Reference protocol implementations

- **Pure ALOHA**: baseline, no collision avoidance
- **LoRaWAN Class A**: ADR, confirmed/unconfirmed uplink, downlink windows
- These serve as correctness anchors and performance baselines

### Phase 4 — Adversary framework

- `Adversary` trait with channel observation and injection API
- Built-in adversaries: replay agent, band saturation agent, selective jammer, passive eavesdropper
- Adversary composition: run N adversaries simultaneously
- Configurable adversary intensity and targeting strategy

### Phase 5 — Metrics & comparison

- Structured metrics output (JSON/CSV)
- Statistical comparison utilities (mean, CDF, confidence intervals)
- Optional dashboard or report generation
- CI integration: regression detection on protocol performance

## Key Design Decisions (open for discussion)

### Sync vs async

**Proposal: sync.** The simulation engine controls time explicitly — there is no benefit to async here, and async adds complexity. Each node's `poll_transmit` is called by the scheduler in deterministic order. Revisit if we need to model real-time wall-clock behavior.

### Discrete-event vs continuous time

**Proposal: discrete-event.** LoRa symbol timing is discrete at the physical layer. Discrete-event simulation is simpler to reason about, deterministic, and fast. Continuous time adds little value for MAC-level analysis.

### Frame representation

**Proposal: typed frames with a generic parameter on `Protocol`.** This avoids byte-buffer casting while keeping the channel generic. The channel can erase the frame type behind a trait object for multi-protocol simulations.

### Adversary visibility

**Proposal: adversaries observe the channel at the physical layer** (pre-collision-resolution), matching real-world capability. They cannot inspect node-internal state unless explicitly modeled as a compromised node.

### Randomness

**Proposal: seeded `rand` with explicit `Rng` threading** through all stochastic components. No global RNG. This makes simulations fully reproducible from a seed and enables parallel runs with different seeds.
