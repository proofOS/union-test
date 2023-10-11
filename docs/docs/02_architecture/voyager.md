---
title: "Voyager"
---

# Voyager

IBC relays on off-chain actors transferring packets and proofs between chains. Voyager is our in-house relayer, allowing us to support new networks without waiting for up-stream support.

## Architecture

We have opted for an event driven architecture, where the application uses an internal memory queue for observed events and I/O.

```mermaid
stateDiagram-v2
    direction LR
    EventListener --> Queue
    Queue --> Processor
    state Processor {
      [*] --> Actor
      Actor --> Galois
      Galois --> [*]
    }

    Processor --> Chains
    Chains --> EventListener
```

Voyager integrates over [gRPC](https://grpc.io/) with Galois to offload computation to dedicated hardware, with pending support for proving markets.