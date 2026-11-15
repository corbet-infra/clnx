# clnx — Linux host drivers for the VM/GPU system

`clnx` implements the `cdrv` **host** traits on Linux. It holds no decisions:
the conductor (`cdct`) decides admission and device ownership; the workload
runtime (`cwld`) asks for the actions; this crate carries them out and reports
facts.

## What it does

| Module | Implements | How |
|---|---|---|
| `instances` | `HostInstances` | Defines libvirt domains rendered from the one source (`InstanceSpec`), starts them, stops them cleanly (guest agent / ACPI, never `destroy`), and translates lifecycle events. |
| `domain_xml` | — | The single renderer: desktop class (VFIO functions, the 64 GiB MMIO window for ReBAR, fixed memory, no balloon), dev class (balloon, free-page reporting), 4Kn on every disk, macOS balloon on `pcie.0`. Pure and snapshot-tested. |
| `devices` | `HostDevices` | Enumerates PCI functions and their **live** IOMMU groups from sysfs, refuses passthrough if a group holds an undeclared device, binds functions `.1 .2 .3` then `.0` (audio/USB/i2c before the GPU) only when no process holds the card's DRM nodes (or `/dev/kfd` for amdgpu) and the host driver's module use count is 0, returns in the proven reverse order (safe to repeat: a function already on its host driver is not bound or probed again), resets once and reports the result, and reports BAR0 size. |
| `memory` | `HostMemory` | Reports `MemAvailable` plus the evictable ARC (`size - c_min` capped) and the PSI `full avg10` share; moves a balloon and reads balloon/RSS figures through libvirt. |
| `gpu` | `HostGpu` | Stops the desktop session at its unit (the container keeps running), scales a GPU pod to zero behind Argo CD's skip-reconcile and restores its own replica count, and moves Sunshine to the kernel that holds the card. |

Every host boundary is a small trait in `backends`: `Libvirt`, `Sysfs`,
`Kernel`, `Systemd`, `Kube`, `Procs`. Tests use fakes and fixture trees (a synthetic
`/sys` directory, a fixture `/proc`, channel-fed events); nothing in the test
suite needs a hypervisor, a cluster or a GPU.

## Boundaries — this crate must never

- decide admission, priority or ownership, or hold a claim,
- destroy a guest except through the contract's explicit `destroy`; a stop is a
  clean guest-agent/ACPI request,
- poll for state: lifecycle changes arrive as events, and a missing event
  source is reported rather than replaced by a loop,
- kill a process that holds the card: the hand-over waits (pidfd, event-driven,
  default 60 s) for the holders to leave and then refuses, naming
  `pid comm node`; it never writes to sysfs while a holder or a busy host
  driver remains,
- retry a failed reset or an unbind the kernel blocked: the failure is
  reported,
- read a declaration instead of the live host for an IOMMU group or a driver,
- carry real host data (PCI addresses, serials, hostnames, paths), Apple
  material (the OSK is supplied by private configuration and defaults to
  absent) or a Windows identity (SMBIOS strings are supplied by the caller).

The only external command this crate runs is `modprobe`; everything else goes
through libvirt, D-Bus, Kubernetes, or a sysfs write.

## Contract changes this driver needs

Listed for `cdrv`/the system owners, not applied here:

1. **Lifecycle events** (resolved here, not in the contract). `virt` 0.5.0 has
   no `virConnectDomainEventRegisterAny` and this crate forbids `unsafe`, so
   `RealLibvirt` streams events from `virsh event --loop`, libvirt's own event
   client (it blocks on the event stream; nothing polls). A native
   registration can replace it behind `DomainEventSource`.
2. **Paused state, BAR0 fact, spec vcpus/nic/console/identity**: delivered by
   the `cdrv` batch 2 contract and used here (`InstanceState::Paused`,
   `DeviceFacts::bar0_bytes`, the spec fields). Still local: machine type,
   firmware paths, OpenCore image, the declared function set of the card.
3. **Guest identity**: the spec carries SMBIOS identity; the AppleSMC OSK is
   never in the spec and arrives only through `RenderOptions`.
4. **Console**: `spec.console` renders a VNC socket (libvirt picks the path
   unless `RenderOptions::console_socket` names one).
5. **Memory parsers.** The pure `/proc/meminfo`, `arcstats` and PSI parsers
   belong in `cdrv` beside `MemoryFacts`. They currently exist in `cdct` and,
   so that a driver does not depend on the conductor, in this crate's private
   `proc` module.
6. **Tenant replica counts** (resolved by cdrv `HostGpu::tenant_replicas` and
   `start_tenant(.., replicas)`): the driver keeps no state; the caller reads
   the count before the stop and passes it back. Zero replicas are refused.
7. **Holder gate.** `bind_passthrough` refuses while a process holds the
   card or `/sys/module/<host_driver>/refcnt` is not 0. The runtime must
   take the desktop session off the card (and wait for that job) before it
   asks for the bind. The `Sysfs` trait gained `list`, and `LinuxDevices`
   gained a `Procs` backend (`with_procs`, `with_holder_wait`).
8. **Storage.** `HostStorage` is not implemented in this iteration; disks are
   rendered from `InstanceSpec`, and creating/resetting them remains `cdsk`'s
   work.

## Proven on a real libvirt without the GPU

`examples/host_proof.rs` drives `LinuxInstances` and `LinuxMemory` against a
live libvirt with a synthetic domain on a throwaway 4Kn disk: define from the
rendered XML, start, lifecycle events, balloon target and guest free pages,
managed save and restore, ACPI clean stop (no `destroy` anywhere). Notes from
that run:

- Events are delivered from the moment `virsh event` has registered; a
  consumer subscribes first and then reads `list()` to close the gap. Event
  order is a hint (a first start showed `Resumed` before `Started`); `state()`
  is the truth.
- A guest with no passed-through device renders a migratable CPU so managed
  save works; a device-holding guest keeps the full host CPU (it cannot save).
- `RenderOptions::emulator` is `None` by default: libvirt picks the host's
  emulator (a bare name is not resolvable on hosts without a conventional PATH).

## What only the real host can prove

- That libvirt accepts the rendered XML (`virt-xml-validate`,
  `domxml-to-native qemu-argv`) and the macOS/Windows firmware paths resolve.
- The real sysfs bind/unbind, the live IOMMU groups, BAR0 size and the
  post-return driver state.
- Balloon and free-page-reporting behaviour in a real macOS/Windows guest,
  including the `pcie.0` placement measured earlier.
- The systemd unit names and the user-bus address of the desktop session.
- Kubernetes RBAC for the dynamic client, Argo CD's annotation actually
  holding a manual scale, and the cluster's real GPU resource names.
- `StopMode::Hibernate` with a VFIO guest (managed save of a GPU-less guest is
  proven on a test host; see `examples/host_proof.rs`).

## How to run the tests

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`k8s-openapi` (pulled by `kube`) needs one API-version feature and cannot
build without it. This driver runs beside one cluster, so it names `v1_34` as
its minimum API server rather than leaving the choice to a consumer.

## License

`LGPL-3.0-only WITH LGPL-3.0-linking-exception`; see `LICENSE`.
