# clnx — Linux host drivers for the VM/GPU system

`clnx` implements the `cdrv` **host** traits on Linux. It holds no decisions:
the conductor (`cdct`) decides admission and device ownership; the workload
runtime (`cwld`) asks for the actions; this crate carries them out and reports
facts.

## What it does

| Module | Implements | How |
|---|---|---|
| `instances` | `HostInstances` | Defines libvirt domains rendered from the one source (`InstanceSpec`), starts them, stops them cleanly (guest agent / ACPI, or a per-instance command for a guest that ignores ACPI; never `destroy`), and translates lifecycle events. |
| `domain_xml` | — | The single renderer: desktop class (VFIO functions, the 64 GiB MMIO window for ReBAR, fixed memory, no balloon), dev class (balloon, free-page reporting), 4Kn on every disk, macOS balloon on `pcie.0`. Pure and snapshot-tested. |
| `devices` | `HostDevices` | Enumerates PCI functions and their **live** IOMMU groups from sysfs, refuses passthrough if a group holds an undeclared device, binds functions `.1 .2 .3` then `.0` (audio/USB/i2c before the GPU) only when no process holds the card's DRM nodes (or `/dev/kfd` for amdgpu; matched by device number, so a process in a container that names the node differently is found too) and the host driver's module use count is 0, returns in the proven reverse order (safe to repeat: a function already on its host driver is not bound or probed again), resets once and reports the result, and reports BAR0 size. |
| `memory` | `HostMemory` | Reports `MemAvailable` plus the evictable ARC (`size - c_min` capped) and the PSI `full avg10` share; moves a balloon and reads balloon/RSS figures through libvirt. |
| `gpu` | `HostGpu` | Stops the desktop session at its unit (the container keeps running; `ZbusSystemd::system_at` reaches a container's own system bus; a start or stop returns only when systemd's job for it has finished with `done`, default deadline 120 s), scales a GPU pod to zero behind Argo CD's skip-reconcile and restores its own replica count, and moves Sunshine to the kernel that holds the card (`TenantConfig::sunshine_unit` is `None` when Sunshine is part of the desktop session unit: it then leaves and returns with that unit and `sunshine_follows` does nothing). |

Every host boundary is a small trait in `backends`: `Libvirt`, `Sysfs`,
`Kernel`, `Systemd`, `Kube`, `Procs`. Tests use fakes and fixture trees (a synthetic
`/sys` directory, a fixture `/proc`, channel-fed events); nothing in the test
suite needs a hypervisor, a cluster or a GPU.

## Boundaries — this crate must never

- decide admission, priority or ownership, or hold a claim,
- destroy a guest except through the contract's explicit `destroy`; a stop is a
  clean guest-agent/ACPI request,
- poll for state: lifecycle changes arrive as events, and a missing event
  source is reported rather than replaced by a loop (one exception: the card
  sampler, item 13, because the GTT counters have no event source at all),
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

The only external commands this crate runs are `modprobe` and the optional per-instance clean-stop command; everything else goes
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
8. **Unit jobs.** `StartUnit`/`StopUnit` return a job path; `ZbusSystemd`
   subscribes to `JobRemoved` before the call and returns only when that job
   is removed. A result other than `done`, or the deadline
   (`with_job_timeout`), is an error naming the unit and the result. Stopping
   the desktop session therefore returns after the whole graphical session is
   down.
9. **Clean stop of a macOS guest.** A macOS guest ignores the ACPI power
   button. `LinuxInstances::with_clean_stop(instance, argv)` names a command
   (no shell) that makes the guest halt itself, for example an ssh
   `sync; sudo -n /sbin/shutdown -h now`. `StopMode::Clean` for that instance
   runs it instead of the libvirt request; its exit status is ignored (the
   remote side cuts the session, ssh exits 255), a spawn failure is an error,
   and the caller still waits for the lifecycle event. The command is private
   deployment data and never part of this repository. Other instances keep
   the guest-agent/ACPI request; `destroy` is never called.
10. **Storage.** `HostStorage` is not implemented in this iteration; disks are
   rendered from `InstanceSpec`, and creating/resetting them remains `cdsk`'s
   work.
11. **BAR limits for a guest that cannot use full ReBAR.** macOS mishandles
   large GPU BARs. OpenCore's manual (acidanthera/OpenCorePkg,
   `Docs/Configuration.tex`, Booter > Quirks > `ResizeAppleGpuBars`): "While
   Apple macOS supports a theoretical 1 GB maximum, in practice all
   non-default values may not work correctly. For this reason the only
   supported value for this quirk is the minimal supported BAR size". The
   guest cannot shrink a BAR itself: vfio-pci gives it the Resizable BAR
   capability read-only. `LinuxDevices::with_bar_limits` therefore shrinks the
   GPU function's BARs on the host, between its unbind and the vfio-pci bind,
   through `resource<N>_resize` (size code = log2 of the size in MiB, checked
   against the BAR's own size mask before anything is written); the host
   driver grows them back when it probes the card on the return. The current
   size is the kernel's `resource` table (`end - start + 1`), never the length
   of the `resource<N>` file: sysfs keeps a file's length from its creation, so
   after amdgpu regrew BAR0 the file still showed the shrunk 256 MiB and the
   shrink was skipped (2026-11-17, attempt 3: QEMU then failed to map a 16 GiB
   BAR). It fails closed: after the shrink, and also when there was nothing to
   shrink, the live size is read back, and a BAR still above its limit refuses
   the passthrough before the GPU function reaches vfio-pci; the whole card is
   handed back to the host driver (the desktop keeps its card). The limits are
   per guest and deployment data (a Windows guest keeps the full BAR).
12. **Device plugin restart** (cdrv `HostGpu::restart_device_plugin`): the
   GPU device plugin's pods on this node are deleted through the Kubernetes
   API (`TenantConfig::device_plugin`: namespace, labels, node) so their
   controller starts fresh ones; a lost registration needs a new process.
13. **Live ladder facts** (`pressure`): read at events.
   Kubernetes watches (this node's GPU pods, the node, the broker Service's
   endpoint slices) and the broker's own event stream (llama-swap
   `/api/events`, `modelStatus` lines only) wake the observer; it then reads
   the card's `mem_info_vram_used`/`_total`/`mem_info_gtt_used`, the GPU
   workloads' ready replicas and rungs, the broker's `/running` and the node's
   allocatable for the guarded resources. The broker is addressed through its
   Service's cluster IP, read at each use. The one clocked source is the
   **card sampler** (`CardSampler`, 2026-11-17: the retired pressure-watcher's
   6 s cadence restored): a desktop's graphics VRAM spills to system RAM (GTT)
   without any event, so it reads the two local counters every interval and
   wakes the observer only while the full card spills (GTT grew by more than
   the noise floor since the previous reading, VRAM above the full fraction;
   consecutive readings are counted as the grace), once when a spill ends, and
   when VRAM crosses the full fraction. No API call is made unless it wakes.

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
