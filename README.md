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
| `devices` | `HostDevices` | Enumerates PCI functions and their **live** IOMMU groups from sysfs, refuses passthrough if a group holds an undeclared device, binds functions `.1 .2 .3` then `.0` (audio/USB/i2c before the GPU), returns in the proven reverse order, resets once and reports the result, and reports BAR0 size. |
| `memory` | `HostMemory` | Reports `MemAvailable` plus the evictable ARC (`size - c_min` capped) and the PSI `full avg10` share; moves a balloon and reads balloon/RSS figures through libvirt. |
| `gpu` | `HostGpu` | Stops the desktop session at its unit (the container keeps running), scales a GPU pod to zero behind Argo CD's skip-reconcile and restores its own replica count, and moves Sunshine to the kernel that holds the card. |

Every host boundary is a small trait in `backends`: `Libvirt`, `Sysfs`,
`Kernel`, `Systemd`, `Kube`. Tests use fakes and fixture trees (a synthetic
`/sys` directory, a fixture `/proc`, channel-fed events); nothing in the test
suite needs a hypervisor, a cluster or a GPU.

## Boundaries — this crate must never

- decide admission, priority or ownership, or hold a claim,
- destroy a guest except through the contract's explicit `destroy`; a stop is a
  clean guest-agent/ACPI request,
- poll for state: lifecycle changes arrive as events, and a missing event
  source is reported rather than replaced by a loop,
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

1. **Lifecycle events.** The adopted `virt` 0.5.0 crate exposes the libvirt
   event loop but not `virConnectDomainEventRegisterAny`. `DomainEventSource`
   is the seam and the translation is complete and tested, but the production
   source needs that binding (in `virt`, or a small `virt-sys` bridge) — a
   driver must not fall back to polling.
2. **A paused state.** `cdrv::InstanceState` has no paused/suspended value.
   A suspend is emitted as an `instance.suspended` warning carrying
   `"state": "suspended"`, and libvirt's paused/PM-suspended states read as
   `Running` (they are live). Add `Paused`.
3. **Device facts.** `HostDevices` cannot report a BAR size. `clnx` exposes
   `LinuxDevices::bar0_size` inherently; `DeviceFacts.bar0_bytes` or a trait
   method would make the ReBAR fact part of the contract.
4. **Instance options.** `InstanceSpec` carries neither the NIC, the console,
   the machine/firmware paths, the vCPU count, the SMBIOS identity, nor the
   declared function set of the card. `RenderOptions` and the `LinuxDevices`
   constructor carry them today; they deserve contract fields or an options
   payload.
5. **Memory parsers.** The pure `/proc/meminfo`, `arcstats` and PSI parsers
   belong in `cdrv` beside `MemoryFacts`. They currently exist in `cdct` and,
   so that a driver does not depend on the conductor, in this crate's private
   `proc` module.
6. **Tenant replica counts.** `HostGpu` has no way to report or recover the
   replica count it scaled away; `clnx` remembers it in memory, and a fresh
   process restores a tenant to one replica. A persisted tenant ledger (or a
   `HostGpu::tenant_replicas`) would make the restore exact after a crash.
7. **Storage.** `HostStorage` is not implemented in this iteration; disks are
   rendered from `InstanceSpec`, and creating/resetting them remains `cdsk`'s
   work.

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
- `StopMode::Hibernate` (libvirt managed save).

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
