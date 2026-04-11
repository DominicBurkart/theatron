# theatron — Architecture

## Project Goal

theatron is a simulation and evaluation framework that models network-level effects (propagation, interference, contention, adversarial scenarios) to compare protocol implementations under controlled, reproducible conditions. Protocol implementations are external. Any `Protocol` trait implementor can be evaluated — different protocols, different implementations of the same protocol, same implementation with different parameters. LoRaWAN via lora-rs is the first validation target. Outputs help clients with stack selection and inform protocol development outside theatron.

## Evaluation Dimensions

theatron targets multiple dimensions of protocol evaluation:

- **Performance under interference**: throughput vs spreading factor, saturated band scenarios, co-channel contention
- **Parameter optimization**: SF, bandwidth, coding rate, and TX power tradeoffs
- **Scalability**: throughput and latency degradation as node count grows
- **Reliability**: packet delivery ratio, retransmission overhead, protocol-specific session establishment metrics (e.g. join success rate in LoRaWAN)
- **Energy efficiency**: time-on-air as a proxy for battery impact
- **Security and resilience**: adversarial scenarios including replay attacks, jamming, band flooding, and eavesdropping; adversaries may be external or internal (compromised nodes)

## Core Abstractions

### `Protocol` trait

The central abstraction. Each MAC protocol implements this trait, which defines how a node processes received frames, generates transmissions, and manages state.

```rust
trait Protocol {
    type Config;
    type State;
    type Metrics;

    fn init(&self, config: Self::Config) -> (Self::State, Option<SimTime>);
    fn on_receive(&self, state: &mut Self::State, frame: RxMetadata, time: SimTime) -> Option<SimTime>;
    fn poll_transmit(&self, state: &mut Self::State, time: SimTime) -> Option<Transmission>;
    fn update(&self, state: &mut Self::State, time: SimTime) -> Option<SimTime>;
    fn metrics(&self, state: &Self::State) -> Self::Metrics;
}
```

See [`src/types.rs`](src/types.rs) for the authoritative definitions of `RxMetadata` and `Transmission`.

`init`, `on_receive`, and `update` each return `Option<SimTime>` — the next simulation time at which the scheduler must call `update` on this node. Returning `None` means the node has no pending timer. This allows the scheduler to use event-driven dispatch (a priority queue keyed on SimTime) rather than polling every node on every tick.

`update` drives timer-based state transitions (e.g. RX1/RX2 window opening in LoRaWAN Class A) without requiring an incoming frame.

#### Two ways external protocol implementations connect to theatron

**Adapter integration**: a thin adapter wraps an external crate (e.g. `lorawan-device`). Protocol logic stays entirely in the external crate; the adapter implements the `Protocol` trait to bridge it into the simulation.

**Direct trait implementation**: a protocol implemented externally against the `Protocol` trait directly, for protocols without an existing crate.

#### Recommended pattern for external implementors: static state machine validation

For protocols implemented directly against the `Protocol` trait, the typestate pattern can encode valid state transitions at the type level:

```rust
struct Idle;
struct Transmitting { started: SimTime }
struct RxWindow1 { tx_end: SimTime }

impl Protocol for MyProtocol<Idle> { ... }
impl Protocol for MyProtocol<Transmitting> { ... }
impl Protocol for MyProtocol<RxWindow1> { ... }
```

Invalid transitions become compile errors. For adapter integrations, correctness comes from the upstream crate's own state machine.

#### LoRaWAN Class A state flow (validation target reference)

The following illustrates the validation target's state machine for reference:

```mermaid
stateDiagram-v2
    [*] --> Idle
    Idle --> Transmitting : poll_transmit returns Some
    Transmitting --> RxWindow1 : TX complete + RECEIVE_DELAY1
    RxWindow1 --> RxWindow2 : no downlink received
    RxWindow1 --> Idle : downlink received
    RxWindow2 --> Idle : downlink received or window closed
```

### `TrafficModel` trait

LoRaWAN and similar protocols do not transmit autonomously — the application layer decides when to send data. A `TrafficModel` provides uplink payloads to a node when it is ready to transmit.

```rust
trait TrafficModel {
    fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>>;
}
```

The scheduler calls `poll_transmit` on the protocol; the protocol (or its adapter) calls `next_payload` on the traffic model to get application data. Built-in models: periodic, Poisson arrival, bursty. Custom models can be provided for application-specific load patterns.

### Validation Target: LoRaWAN via lora-rs

LoRaWAN via lora-rs is the first real-world protocol used to prove the simulation engine works with a real stack. The validation example is external to theatron core and comprises three components:

- **LoRaWAN device adapter**: wraps `lorawan-device::nb_device` to implement `NodeHandle` (see [`examples/lorawan_file_transfer/lorawan_adapter.rs`](examples/lorawan_file_transfer/lorawan_adapter.rs))
- **Simulated network server**: passively accumulates received uplink fragments (see [`examples/lorawan_file_transfer/network_server.rs`](examples/lorawan_file_transfer/network_server.rs))
- **SimulatedRadio**: bridges the adapter to theatron's channel (see [`examples/lorawan_file_transfer/simulated_radio.rs`](examples/lorawan_file_transfer/simulated_radio.rs))

The validation example uses:

- **`lorawan`**: frame parsing and creation, MIC verification, MAC command handling. `RxMetadata.payload` and `Transmission.payload` are raw bytes parsed via `lorawan::parser::PhyPayload`.
- **`lorawan-device`**: real Class A state machine via `nb_device`. The adapter drives it using ABP credentials (no over-the-air join), calling `device.send(...)`, `device.handle_event(TimeoutFired)`, and `device.get_radio().inject_downlink(...)`.
- **`lora-modulation`**: SF, bandwidth, and time-on-air calculations. Used to compute `Transmission.duration_us` in `SimulatedRadio`.

#### `nb_device::radio::PhyRxTx` — the actual interface

`lorawan-device`'s `nb_device` module exposes an event-driven radio trait. `SimulatedRadio` implements `PhyRxTx` with `PhyEvent = ()`, `PhyError = &'static str`, and `PhyResponse = ()`.

Events: `TxRequest(TxConfig, &[u8])`, `RxRequest(RfConfig)`, `CancelRx`, `Phy(PhyEvent)`.
Responses: `Idle`, `Txing`, `TxDone(ms)`, `Rxing`, `RxDone(RxQuality)`.

On `TxRequest`, `SimulatedRadio` builds a `Transmission` (computing `duration_us` via `lora-modulation`), stores it as `pending_tx`, and returns `TxDone(0)`. On `RxRequest`, it stores the `RfConfig` and returns `Rxing`. On `Phy(())`, it checks for a pending downlink and returns `RxDone` or `Idle`.

#### SimulatedRadio

`SimulatedRadio` maintains an internal receive buffer and current RX config. Frames arriving via `inject_downlink` are accepted only when the radio is in RX mode and the SF/frequency match; frames that don't match are dropped (physically correct). See [`examples/lorawan_file_transfer/simulated_radio.rs`](examples/lorawan_file_transfer/simulated_radio.rs) for the implementation.

#### Adapter state ownership

`lorawan-device::nb_device::Device<R, RNG, N>` bundles the radio, RNG, and MAC state into a single struct. `LoRaWanAdapter` (see [`examples/lorawan_file_transfer/lorawan_adapter.rs`](examples/lorawan_file_transfer/lorawan_adapter.rs)) holds the device directly alongside theatron bookkeeping:

- `pending_timeout_ms` — set from `Response::TimeoutRequest(ms)`; drives the next `SimTime` wake
- `tx_start_time` — records when the current TX began, so RX window wake times can be computed as `tx_start_time + delay_ms * 1_000`

`NodeHandle` methods on `LoRaWanAdapter` drive the device by calling `device.send(...)`, `device.handle_event(TimeoutFired)`, and `device.get_radio().inject_downlink(...)`.

#### Timer contract

`lorawan-device` returns `Response::TimeoutRequest(delay_ms)` when it needs to be woken after a delay (RX1 window, RX2 window, ACK timeout). The adapter converts this to a `SimTime` and returns it from `update` / `on_receive`. The scheduler inserts this wake time into its event queue and calls `update` at exactly that simulated time, which then delivers `Event::TimeoutFired` to the device.

#### Simulated network server

LoRaWAN is not peer-to-peer — a server must handle joins and schedule downlinks. The current validation example includes a minimal `NetworkServer` (see [`examples/lorawan_file_transfer/network_server.rs`](examples/lorawan_file_transfer/network_server.rs)) that passively accumulates received uplink fragments. The device uses ABP (no over-the-air join) so no join-accept logic is needed in this example. A future example with OTAA would require a server that generates join-accept frames and schedules downlinks into RX1/RX2 windows.

The server implements `NodeHandle` and participates in the simulation as a node. It is part of the lora-rs validation example, not theatron core — consistent with the principle that protocol logic lives outside theatron.

### Channel / Medium

A shared simulation object that models the physical wireless channel: propagation delay, collision detection, RSSI and SNR derivation, SF orthogonality approximation, and time-on-air gating. The channel model is parameterized; in the validation case it is configured for LoRa using `lora-modulation`. The channel carries `Vec<u8>` payloads alongside `TxMetadata` (SF, bandwidth, frequency, TX power). Protocol adapters parse the raw bytes via their respective crates; the channel remains format-agnostic.

All communication flows through the channel — protocols and interference sources do not interact directly.

### Interference Models

Interference sources are first-class simulation participants. They observe the channel subject to the same physical constraints as legitimate nodes and may inject frames or noise. Multiple interference sources can run simultaneously. Each implements an `InterferenceSource` trait.

```rust
trait InterferenceSource {
    fn observe(&mut self, event: &ChannelEvent, time: SimTime);
    fn poll_inject(&mut self, time: SimTime) -> Option<Transmission>;
    fn next_poll_time(&self, current_time: SimTime) -> Option<SimTime>;
}
```

Planned interference models:
- **Saturated band**: high-volume legitimate-looking traffic overwhelming the channel
- **Periodic interferer**: burst interference on a regular schedule (models co-channel ISM band users)
- **Co-channel contention**: multiple independent LoRa networks sharing a frequency plan
- **Adversarial replay**: capture and re-transmit valid frames
- **Selective jamming**: targeted interference against specific SFs or node addresses
- **Passive eavesdropper**: traffic analysis without injection

### Metrics collection

A passive observer attached to the simulation that records per-protocol, per-run statistics: throughput (frames/s per SF), PDR, latency distribution, time-on-air, retransmission count, protocol-specific session establishment metrics (e.g. join success rate in LoRaWAN), and protocol-specific counters. Output in a structured format suitable for statistical comparison across runs.

### Hardware measurement tooling (potential expansion)

To ground simulations in real-world conditions, theatron may include tooling for capturing LoRa hardware connection characteristics — RSSI profiles, SNR distributions, interference patterns, and timing measurements from physical deployments. These measurements would be uploaded as empirical channel model inputs, allowing simulations to reflect actual deployment conditions.

## Phased Roadmap

### Phase 1 — Core simulation engine (validated with LoRaWAN Class A)

- Discrete-event time model (`SimTime` as a microsecond-resolution monotonic counter; see [SimTime resolution](#simtime-resolution))
- Channel model: parameterized propagation, collision detection, RSSI/SNR derivation (configured for LoRa via `lora-modulation`)
- Simulation scheduler (priority queue on SimTime; event-driven dispatch via `Option<SimTime>` returns from `Protocol` methods)
- `Protocol` trait, `TrafficModel` trait, and `SimulatedRadio` bridge
- *Validation*: LoRaWAN Class A adapter wrapping `lorawan-device::nb_device`, plus minimal network server — both as external examples
- Interference models: saturated band, periodic interferer
- Metrics: throughput, PDR, time-on-air
- **Integration test**: SF7–SF12 under clean, saturated, and periodic-interference channel conditions

### Phase 2 — Multi-protocol comparison

- Pure ALOHA as trivial reference implementation for multi-protocol validation
- Multi-protocol simulation: run N protocol instances in the same channel simultaneously
- Comparison output: side-by-side metrics across protocol variants and parameterizations

### Phase 3 — Expanded interference and adversarial models

- Adversarial replay, selective jamming, passive eavesdropper
- Co-channel contention modeling
- Configurable interference intensity and targeting strategy

### Phase 4 — Metrics, parameter sweeps, reporting

- Structured metrics output (JSON/CSV)
- Statistical utilities (mean, CDF, confidence intervals)
- Parameter sweep runner: iterate over SF, bandwidth, node count, interference intensity
- CI integration: regression detection on protocol performance

### Phase 5 — Framework generalization and extended tooling

- Parameterizable channel models beyond LoRa
- Hardware measurement tooling: capture real LoRa hardware characteristics (RSSI profiles, interference patterns, timing) for upload as empirical channel model inputs
- Typestate validation helpers for external protocol implementors
- Optional report generation and dashboard

## Key Design Decisions (open for discussion)

### Sync vs async

**Proposal: sync.** The simulation engine controls time explicitly — there is no benefit to async here, and async adds complexity. Each node's `poll_transmit` is called by the scheduler in deterministic order. `lorawan-device::nb_device` is the correct integration target (not `async_device`) for the same reason. Revisit if we need to model real-time wall-clock behavior.

### Discrete-event vs continuous time

**Proposal: discrete-event.** Wireless symbol timing (e.g. LoRa) is discrete at the physical layer. Discrete-event simulation is simpler to reason about, deterministic, and fast. Continuous time adds little value for MAC-level analysis.

### SimTime resolution

**`SimTime` is a microsecond-resolution monotonic `u64` counter.** Microseconds are required for `lora-modulation`'s time-on-air calculations (which return `u64` microseconds) and for precise collision detection at high SFs. `lorawan-device` timer delays are `u32` milliseconds; conversion is `sim_time_us = delay_ms as u64 * 1_000` (see [`src/time.rs`](src/time.rs)). Symbol times at SF7/125kHz are ~1ms; time-on-air at SF12/125kHz is ~2.5s — both fit comfortably in microsecond `u64`.

### Frame representation

**Concrete: the channel carries `Vec<u8>` + `TxMetadata`.** Protocol adapters use their respective crates (e.g. `lorawan` for LoRaWAN) to parse and construct frames. The channel stays format-agnostic; type safety lives at the protocol layer, not the channel layer.

### Interference source visibility

**Proposal: interference sources observe the channel at the physical layer** (pre-collision-resolution), matching real-world RF capability. They cannot inspect node-internal state unless explicitly modeled as compromised nodes.

### Protocol logic lives outside theatron

**Principle: theatron's value is the simulation engine, channel model, and evaluation infrastructure.** Protocol implementations — whether adapting existing crates or built from scratch — are external. theatron provides the `Protocol` trait contract and the simulated medium; protocol authors provide the state machines. The lora-rs validation example (device adapter, network server, SimulatedRadio) ships alongside theatron as an example, not as part of the core library.

### Randomness

**Proposal: seeded RNG with explicit threading** through all stochastic components. No global RNG. This makes simulations fully reproducible from a seed and enables parallel runs with different seeds. The LoRaWAN adapter uses `Xorshift64` (see [`examples/lorawan_file_transfer/prng.rs`](examples/lorawan_file_transfer/prng.rs)), initialized per-node from a per-node seed derived from the master simulation seed.
