//! Render a libvirt domain from an [`InstanceSpec`].
//!
//! The spec is the one source: this module replaces the Nix domain renderers,
//! so the class rules (fixed memory and no balloon for a GPU desktop, balloon
//! for a dev VM, 4Kn everywhere) are applied here rather than in a host's
//! deployment code. Deployment data the spec does not carry (machine type,
//! firmware paths, the guest's NIC, console socket) arrives in
//! [`RenderOptions`]; the README lists those as contract gaps.
//!
//! The renderer is pure and deterministic: the same spec and options always
//! render the same XML, so a fixture snapshot is a meaningful test.

use cdrv::{
    BlockDevice, DeviceAddress, GuestOs, InstanceClass, InstanceKind, InstanceSpec, MemoryClaim,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::error::{ClnxError, Result};

/// Deployment data the instance spec does not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOptions {
    /// The QEMU machine type.
    pub machine: String,
    /// The emulator binary, as an absolute path. `None` lets libvirt choose
    /// the host's default emulator (a bare name is not resolved on hosts
    /// whose libvirt has no conventional `PATH`).
    pub emulator: Option<String>,
    /// The pflash code file, when the firmware needs an explicit loader.
    pub loader: Option<PathBuf>,
    /// The NVRAM template, when the firmware needs an explicit template.
    pub nvram_template: Option<PathBuf>,
    /// The per-domain NVRAM file.
    pub nvram: Option<PathBuf>,
    /// The OpenCore boot image of a macOS guest.
    pub opencore_iso: Option<PathBuf>,
    /// The AppleSMC OSK string for a macOS guest; never defaulted in this repo.
    pub apple_smc_osk: Option<String>,
    /// A custom CPU model string, rendered through the QEMU command line.
    pub cpu_model: Option<String>,
    /// The 64-bit MMIO window in MiB, so OVMF can place a large BAR.
    pub mmio64_mib: u64,
    /// The VNC socket rendered when the spec asks for a console.
    pub console_socket: Option<PathBuf>,
    /// Whether to keep an emulated display beside a passed-through card.
    pub emulated_display: bool,
    /// Extra MiB above the RAM claim for the domain's hard limit.
    pub hard_limit_extra_mib: Option<u64>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            machine: "pc-q35-11.1".to_owned(),
            emulator: None,
            loader: None,
            nvram_template: None,
            nvram: None,
            opencore_iso: None,
            apple_smc_osk: None,
            cpu_model: None,
            mmio64_mib: 65_536,
            console_socket: None,
            emulated_display: true,
            hard_limit_extra_mib: Some(4096),
        }
    }
}

/// The vCPU count when the spec does not name one.
const DEFAULT_VCPUS: u32 = 8;

fn vcpus(spec: &InstanceSpec) -> u32 {
    spec.vcpus.unwrap_or(DEFAULT_VCPUS)
}

/// Render the libvirt domain XML for one instance.
///
/// # Errors
///
/// Refuses a spec that is inconsistent with the platform rules: a non-4Kn
/// disk, a balloon on a claim a passed-through device has pinned, a claim
/// whose fixed range is not fixed, or a passthrough set that cannot be placed
/// on a guest PCI slot.
pub fn render_domain(spec: &InstanceSpec, options: &RenderOptions) -> Result<String> {
    validate(spec)?;
    let macos = spec.guest_os == GuestOs::MacOs;
    let windows = spec.guest_os == GuestOs::Windows;
    let mut xml = Xml::default();

    xml.line(
        0,
        "<domain type='kvm' xmlns:qemu='http://libvirt.org/schemas/domain/qemu/1.0'>",
    );
    xml.line(1, &format!("<name>{}</name>", escape(spec.id.as_str())));
    xml.line(
        1,
        &format!("<memory unit='MiB'>{}</memory>", spec.memory.max_mib),
    );
    xml.line(
        1,
        &format!(
            "<currentMemory unit='MiB'>{}</currentMemory>",
            spec.memory.max_mib
        ),
    );
    if let Some(extra) = options
        .hard_limit_extra_mib
        .filter(|_| !spec.memory.balloon)
    {
        xml.line(1, "<memtune>");
        xml.line(
            2,
            &format!(
                "<hard_limit unit='MiB'>{}</hard_limit>",
                spec.memory.max_mib + extra
            ),
        );
        xml.line(1, "</memtune>");
    }
    xml.line(
        1,
        &format!("<vcpu placement='static'>{}</vcpu>", vcpus(spec)),
    );
    render_sysinfo(&mut xml, spec, macos);
    render_os(&mut xml, spec, options, macos);
    render_features(&mut xml, windows);
    render_cpu(&mut xml, spec, options, macos);
    render_clock(&mut xml, windows);
    xml.line(1, "<on_poweroff>destroy</on_poweroff>");
    xml.line(1, "<on_reboot>restart</on_reboot>");
    xml.line(1, "<on_crash>destroy</on_crash>");
    xml.line(1, "<devices>");
    if let Some(emulator) = &options.emulator {
        xml.line(2, &format!("<emulator>{}</emulator>", escape(emulator)));
    }
    let slots = render_disks(&mut xml, spec, options, macos);
    render_hostdevs(&mut xml, spec, &slots);
    render_interfaces(&mut xml, spec);
    render_console(&mut xml, spec, options);
    render_inputs(&mut xml, macos, windows);
    render_video(&mut xml, options, macos);
    render_balloon(&mut xml, spec, macos);
    render_tpm(&mut xml, windows);
    if !macos {
        xml.line(2, "<channel type='unix'>");
        xml.line(3, "<target type='virtio' name='org.qemu.guest_agent.0'/>");
        xml.line(2, "</channel>");
    }
    xml.line(1, "</devices>");
    render_commandline(&mut xml, spec, options, macos);
    xml.line(0, "</domain>");
    Ok(xml.0)
}

/// Validate the spec against the platform rules before rendering anything.
fn validate(spec: &InstanceSpec) -> Result<()> {
    if spec.kind != InstanceKind::Vm {
        return Err(ClnxError::Unsupported(
            "clnx defines VMs as libvirt domains; containers and jails are other drivers"
                .to_owned(),
        ));
    }
    let id = spec.id.as_str();
    if id.is_empty()
        || id.contains(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '&' | '\'' | '"'))
    {
        return Err(ClnxError::InvalidSpec(
            "the instance id must be non-empty and free of whitespace and XML punctuation"
                .to_owned(),
        ));
    }
    if spec.vcpus == Some(0) {
        return Err(ClnxError::InvalidSpec(
            "vcpus must be at least 1".to_owned(),
        ));
    }
    if spec.memory.min_mib == 0 || spec.memory.min_mib > spec.memory.max_mib {
        return Err(ClnxError::InvalidSpec(format!(
            "the memory claim {}-{} MiB is not a range",
            spec.memory.min_mib, spec.memory.max_mib
        )));
    }
    if !spec.memory.balloon && spec.memory.min_mib != spec.memory.max_mib {
        return Err(ClnxError::InvalidSpec(
            "a fixed claim needs min_mib == max_mib".to_owned(),
        ));
    }
    if !spec.devices.is_empty() && spec.memory.balloon {
        return Err(ClnxError::InvalidSpec(
            "a passed-through device pins all guest memory, so the claim cannot balloon".to_owned(),
        ));
    }
    for disk in &spec.disks {
        if !disk.sector.is_four_kn() {
            return Err(ClnxError::Refused(format!(
                "4Kn always: {} presents a {} / {} byte sector",
                disk.path.display(),
                disk.sector.logical,
                disk.sector.physical
            )));
        }
    }
    if spec.disks.len() > 25 {
        return Err(ClnxError::InvalidSpec(
            "at most 25 disks plus the OpenCore image fit the SATA name space".to_owned(),
        ));
    }
    validate_slots(&spec.devices)
}

/// Group passed-through functions by host slot and check the guest placement.
fn validate_slots(devices: &[DeviceAddress]) -> Result<()> {
    let mut slots: BTreeMap<(u16, u8, u8), Vec<u8>> = BTreeMap::new();
    for device in devices {
        if device.function > 7 {
            return Err(ClnxError::InvalidSpec(format!(
                "PCI function {} is outside 0..=7",
                device.function
            )));
        }
        slots
            .entry((device.domain, device.bus, device.slot))
            .or_default()
            .push(device.function);
    }
    for ((_, bus, slot), functions) in slots {
        if !functions.contains(&0) {
            return Err(ClnxError::InvalidSpec(format!(
                "the passed-through slot {bus:02x}:{slot:02x} has no function 0"
            )));
        }
        let mut seen = functions.clone();
        seen.sort_unstable();
        seen.dedup();
        if seen.len() != functions.len() {
            return Err(ClnxError::InvalidSpec(format!(
                "the passed-through slot {bus:02x}:{slot:02x} names a function twice"
            )));
        }
    }
    Ok(())
}

fn render_sysinfo(xml: &mut Xml, spec: &InstanceSpec, macos: bool) {
    if macos {
        xml.line(1, "<sysinfo type='smbios'>");
        xml.line(2, "<system>");
        xml.line(3, "<entry name='manufacturer'>Apple Inc.</entry>");
        xml.line(3, "<entry name='product'>MacPro7,1</entry>");
        xml.line(3, "<entry name='version'>1.0</entry>");
        xml.line(2, "</system>");
        xml.line(2, "<baseBoard>");
        xml.line(3, "<entry name='manufacturer'>Apple Inc.</entry>");
        xml.line(3, "<entry name='product'>Mac-27AD2F918AE68F61</entry>");
        xml.line(3, "<entry name='version'>1.0</entry>");
        xml.line(2, "</baseBoard>");
        xml.line(1, "</sysinfo>");
    } else if let Some(smbios) = &spec.identity {
        xml.line(1, "<sysinfo type='smbios'>");
        xml.line(2, "<system>");
        xml.line(
            3,
            &format!(
                "<entry name='manufacturer'>{}</entry>",
                escape(&smbios.manufacturer)
            ),
        );
        xml.line(
            3,
            &format!("<entry name='product'>{}</entry>", escape(&smbios.product)),
        );
        xml.line(
            3,
            &format!("<entry name='version'>{}</entry>", escape(&smbios.version)),
        );
        if let Some(uuid) = &smbios.system_uuid {
            xml.line(3, &format!("<entry name='uuid'>{}</entry>", escape(uuid)));
        }
        xml.line(2, "</system>");
        if let (Some(manufacturer), Some(product)) =
            (&smbios.board_manufacturer, &smbios.board_product)
        {
            xml.line(2, "<baseBoard>");
            xml.line(
                3,
                &format!(
                    "<entry name='manufacturer'>{}</entry>",
                    escape(manufacturer)
                ),
            );
            xml.line(
                3,
                &format!("<entry name='product'>{}</entry>", escape(product)),
            );
            if let Some(version) = &smbios.board_version {
                xml.line(
                    3,
                    &format!("<entry name='version'>{}</entry>", escape(version)),
                );
            }
            xml.line(2, "</baseBoard>");
        }
        xml.line(1, "</sysinfo>");
    }
}

fn render_os(xml: &mut Xml, spec: &InstanceSpec, options: &RenderOptions, macos: bool) {
    let sysinfo = macos || spec.identity.is_some();
    xml.line(1, "<os firmware='efi'>");
    xml.line(
        2,
        &format!(
            "<type arch='x86_64' machine='{}'>hvm</type>",
            escape(&options.machine)
        ),
    );
    xml.line(2, "<firmware>");
    xml.line(3, "<feature enabled='no' name='secure-boot'/>");
    xml.line(3, "<feature enabled='no' name='enrolled-keys'/>");
    xml.line(2, "</firmware>");
    if let Some(loader) = &options.loader {
        xml.line(
            2,
            &format!(
                "<loader readonly='yes' type='pflash' format='raw'>{}</loader>",
                escape(&loader.display().to_string())
            ),
        );
    }
    if let Some(template) = &options.nvram_template
        && let Some(nvram) = &options.nvram
    {
        xml.line(
            2,
            &format!(
                "<nvram template='{}' templateFormat='raw' format='raw'>{}</nvram>",
                escape(&template.display().to_string()),
                escape(&nvram.display().to_string())
            ),
        );
    }
    if sysinfo {
        xml.line(2, "<smbios mode='sysinfo'/>");
    }
    xml.line(1, "</os>");
}

fn render_features(xml: &mut Xml, windows: bool) {
    xml.line(1, "<features>");
    xml.line(2, "<acpi/>");
    xml.line(2, "<apic/>");
    if windows {
        xml.line(2, "<hyperv mode='custom'>");
        for name in [
            "relaxed",
            "vapic",
            "vpindex",
            "runtime",
            "synic",
            "stimer",
            "reset",
            "vendor_id",
            "frequencies",
            "reenlightenment",
            "tlbflush",
            "ipi",
        ] {
            xml.line(3, &format!("<{name} state='on'/>"));
        }
        xml.line(3, "<spinlocks state='on' retries='8191'/>");
        xml.line(2, "</hyperv>");
        xml.line(2, "<vmport state='off'/>");
        xml.line(2, "<smm state='on'/>");
        xml.line(2, "<ioapic driver='kvm'/>");
    }
    xml.line(1, "</features>");
}

fn render_cpu(xml: &mut Xml, spec: &InstanceSpec, options: &RenderOptions, macos: bool) {
    if macos && options.cpu_model.is_some() {
        // The model travels through the QEMU command line, as the proven
        // macOS domain does; a `<cpu>` element beside it would duplicate -cpu.
        return;
    }
    match &options.cpu_model {
        Some(model) => {
            xml.line(1, "<cpu mode='custom' match='exact' check='none'>");
            xml.line(
                2,
                &format!("<model fallback='forbid'>{}</model>", escape(model)),
            );
            render_topology(xml, vcpus(spec));
            xml.line(1, "</cpu>");
        }
        None => {
            // A guest without passed-through devices stays savable (managed
            // save needs a migratable CPU, which drops `invtsc`); a guest that
            // pins a device can neither migrate nor save, so it keeps the
            // full host CPU.
            let migratable = if spec.devices.is_empty() { "on" } else { "off" };
            xml.line(
                1,
                &format!("<cpu mode='host-passthrough' check='none' migratable='{migratable}'>"),
            );
            render_topology(xml, vcpus(spec));
            xml.line(1, "</cpu>");
        }
    }
}

fn render_topology(xml: &mut Xml, vcpus: u32) {
    xml.line(
        2,
        &format!("<topology sockets='1' dies='1' clusters='1' cores='{vcpus}' threads='1'/>"),
    );
}

fn render_clock(xml: &mut Xml, windows: bool) {
    let offset = if windows { "localtime" } else { "utc" };
    xml.line(1, &format!("<clock offset='{offset}'>"));
    xml.line(2, "<timer name='rtc' tickpolicy='catchup'/>");
    if windows {
        xml.line(2, "<timer name='hypervclock' present='yes'/>");
        xml.line(2, "<timer name='pit' tickpolicy='discard'/>");
    } else {
        xml.line(2, "<timer name='pit' tickpolicy='delay'/>");
    }
    xml.line(2, "<timer name='hpet' present='no'/>");
    xml.line(1, "</clock>");
}

/// Render the disks and return the passed-through slots keyed by host slot.
fn render_disks(
    xml: &mut Xml,
    spec: &InstanceSpec,
    options: &RenderOptions,
    macos: bool,
) -> BTreeMap<(u16, u8, u8), u8> {
    let sata = macos;
    let mut order = 1;
    let mut index = 0;
    if macos && let Some(iso) = &options.opencore_iso {
        xml.line(2, "<disk type='file' device='cdrom'>");
        xml.line(3, "<driver name='qemu' type='raw'/>");
        xml.line(
            3,
            &format!("<source file='{}'/>", escape(&iso.display().to_string())),
        );
        xml.line(3, "<target dev='sda' bus='sata'/>");
        xml.line(3, "<readonly/>");
        xml.line(3, &format!("<boot order='{order}'/>"));
        xml.line(2, "</disk>");
        order += 1;
        index = 1;
    }
    for disk in &spec.disks {
        if macos {
            // macOS disks are emulated NVMe, 4096/4096 (render_commandline): QEMU
            // refuses 4096-byte sectors on SATA/IDE. Boot order follows the CD.
            order += 1;
            index += 1;
            continue;
        }
        render_disk(xml, disk, sata, order, index);
        order += 1;
        index += 1;
    }
    if sata && options.opencore_iso.is_some() {
        xml.line(2, "<controller type='sata' index='0'/>");
    }
    // One guest slot per distinct host slot, in address order; the functions
    // of one card share a slot, as the proven passthrough domain places them.
    let host_slots: std::collections::BTreeSet<(u16, u8, u8)> = spec
        .devices
        .iter()
        .map(|device| (device.domain, device.bus, device.slot))
        .collect();
    host_slots
        .into_iter()
        .enumerate()
        .map(|(index, host)| (host, u8::try_from(index).unwrap_or(0)))
        .collect()
}

fn render_disk(xml: &mut Xml, disk: &BlockDevice, sata: bool, order: usize, index: usize) {
    let letters = if sata { "sd" } else { "vd" };
    let letter = char::from(b'a' + u8::try_from(index).unwrap_or(0));
    let dev = format!("{letters}{letter}");
    xml.line(2, "<disk type='block' device='disk'>");
    xml.line(
        3,
        "<driver name='qemu' type='raw' cache='none' discard='unmap'/>",
    );
    xml.line(
        3,
        &format!(
            "<source dev='{}'/>",
            escape(&disk.path.display().to_string())
        ),
    );
    if sata {
        xml.line(
            3,
            &format!("<target dev='{dev}' bus='sata' rotation_rate='1'/>"),
        );
    } else {
        xml.line(3, &format!("<target dev='{dev}' bus='virtio'/>"));
    }
    xml.line(
        3,
        "<blockio logical_block_size='4096' physical_block_size='4096'/>",
    );
    if let Some(serial) = &disk.serial {
        xml.line(3, &format!("<serial>{}</serial>", escape(serial)));
    }
    if disk.read_only {
        xml.line(3, "<readonly/>");
    }
    xml.line(3, &format!("<boot order='{order}'/>"));
    xml.line(2, "</disk>");
}

fn render_hostdevs(xml: &mut Xml, spec: &InstanceSpec, slots: &BTreeMap<(u16, u8, u8), u8>) {
    if spec.devices.is_empty() {
        return;
    }
    xml.line(2, "<controller type='pci' index='0' model='pcie-root'/>");
    for slot in 0..slots.len() {
        let index = slot + 1;
        xml.line(
            2,
            &format!("<controller type='pci' index='{index}' model='pcie-root-port'>"),
        );
        xml.line(3, "<model name='pcie-root-port'/>");
        xml.line(
            3,
            &format!("<target chassis='{index}' port='0x{:02x}'/>", 0x10 + slot),
        );
        xml.line(2, "</controller>");
    }
    let mut devices = spec.devices.clone();
    devices.sort_unstable();
    for device in &devices {
        let slot = slots
            .get(&(device.domain, device.bus, device.slot))
            .copied()
            .unwrap_or(0);
        let multifunction = if device.function == 0 {
            " multifunction='on'"
        } else {
            ""
        };
        xml.line(2, "<hostdev mode='subsystem' type='pci' managed='no'>");
        xml.line(3, "<driver name='vfio'/>");
        xml.line(3, "<source>");
        xml.line(
            4,
            &format!(
                "<address domain='0x{:04x}' bus='0x{:02x}' slot='0x{:02x}' function='0x{:x}'/>",
                device.domain, device.bus, device.slot, device.function
            ),
        );
        xml.line(3, "</source>");
        xml.line(
            3,
            &format!(
                "<address type='pci' domain='0x0000' bus='0x{:02x}' slot='0x00' function='0x{:x}'{multifunction}/>",
                slot + 1,
                device.function
            ),
        );
        xml.line(2, "</hostdev>");
    }
}

fn render_interfaces(xml: &mut Xml, spec: &InstanceSpec) {
    if let Some(nic) = &spec.nic {
        xml.line(2, "<interface type='user'>");
        xml.line(3, &format!("<mac address='{}'/>", escape(&nic.mac)));
        xml.line(
            3,
            &format!(
                "<model type='{}'/>",
                escape(nic.model.as_deref().unwrap_or("virtio"))
            ),
        );
        xml.line(2, "</interface>");
    }
}

fn render_console(xml: &mut Xml, spec: &InstanceSpec, options: &RenderOptions) {
    if !spec.console {
        return;
    }
    match &options.console_socket {
        Some(socket) => xml.line(
            2,
            &format!(
                "<graphics type='vnc' socket='{}'/>",
                escape(&socket.display().to_string())
            ),
        ),
        None => {
            // libvirt picks the socket path itself.
            xml.line(2, "<graphics type='vnc'>");
            xml.line(3, "<listen type='socket'/>");
            xml.line(2, "</graphics>");
        }
    }
}

fn render_inputs(xml: &mut Xml, macos: bool, windows: bool) {
    if macos {
        xml.line(2, "<input type='keyboard' bus='ps2'/>");
        xml.line(2, "<input type='mouse' bus='ps2'/>");
    } else if windows {
        xml.line(2, "<input type='tablet' bus='usb'/>");
        xml.line(2, "<input type='mouse' bus='ps2'/>");
        xml.line(2, "<input type='keyboard' bus='ps2'/>");
    }
    xml.line(2, "<audio id='1' type='none'/>");
}

fn render_video(xml: &mut Xml, options: &RenderOptions, macos: bool) {
    if !options.emulated_display {
        xml.line(2, "<video>");
        xml.line(3, "<model type='none'/>");
        xml.line(2, "</video>");
        return;
    }
    if macos {
        xml.line(2, "<video>");
        xml.line(
            3,
            "<model type='vmvga' vram='65536' heads='1' primary='yes'/>",
        );
        xml.line(2, "</video>");
    } else {
        xml.line(2, "<video>");
        xml.line(
            3,
            "<model type='virtio' device='virtio-vga' heads='1' primary='yes'/>",
        );
        xml.line(2, "</video>");
    }
}

fn render_balloon(xml: &mut Xml, spec: &InstanceSpec, macos: bool) {
    if !spec.memory.balloon {
        xml.line(2, "<memballoon model='none'/>");
        return;
    }
    xml.line(
        2,
        "<memballoon model='virtio' autodeflate='on' freePageReporting='on'>",
    );
    xml.line(3, "<stats period='10'/>");
    if macos {
        // Measured: a macOS guest sees the virtio balloon only on pcie.0
        // (bus 0x00); behind a root port it is invisible to the guest.
        xml.line(
            3,
            "<address type='pci' domain='0x0000' bus='0x00' slot='0x13' function='0x0'/>",
        );
    }
    xml.line(2, "</memballoon>");
}

fn render_tpm(xml: &mut Xml, windows: bool) {
    if windows {
        xml.line(2, "<tpm model='tpm-tis'>");
        xml.line(
            3,
            "<backend type='emulator' version='2.0' persistent_state='yes'/>",
        );
        xml.line(2, "</tpm>");
    }
}

fn render_commandline(xml: &mut Xml, spec: &InstanceSpec, options: &RenderOptions, macos: bool) {
    let mut args: Vec<String> = Vec::new();
    if macos {
        if let Some(osk) = &options.apple_smc_osk {
            args.push("-device".to_owned());
            args.push(format!("isa-applesmc,osk={osk}"));
        }
        if let Some(model) = &options.cpu_model {
            args.push("-cpu".to_owned());
            args.push(model.clone());
        }
    }
    if macos {
        // First guest bootindex after the OpenCore CD (1); pcie.0 slots 0x14.. (0x12 is the
        // NIC, 0x13 the balloon, measured). discard + detect-zeroes let APFS TRIM free the zvol.
        let first_boot = if options.opencore_iso.is_some() { 2 } else { 1 };
        for (n, disk) in spec.disks.iter().enumerate() {
            args.push("-drive".to_owned());
            args.push(format!(
                "if=none,id=nvm{n},file={},format=raw,cache=none,aio=native,discard=unmap,detect-zeroes=unmap{}",
                disk.path.display(),
                if disk.read_only { ",readonly=on" } else { "" }
            ));
            args.push("-device".to_owned());
            args.push(format!(
                "nvme,drive=nvm{n},serial={},logical_block_size=4096,physical_block_size=4096,discard_granularity=16384,bus=pcie.0,addr=0x{:x},bootindex={}",
                disk.serial.clone().unwrap_or_else(|| format!("MACNVME{n}")),
                0x14 + n,
                first_boot + n
            ));
        }
    }
    let large_bar =
        spec.class == InstanceClass::Desktop && !spec.devices.is_empty() && options.mmio64_mib > 0;
    if large_bar {
        args.push("-fw_cfg".to_owned());
        args.push(format!(
            "opt/ovmf/X-PciMmio64Mb,string={}",
            options.mmio64_mib
        ));
    }
    if args.is_empty() {
        return;
    }
    xml.line(1, "<qemu:commandline>");
    for arg in args {
        xml.line(2, &format!("<qemu:arg value='{}'/>", escape(&arg)));
    }
    xml.line(1, "</qemu:commandline>");
}

/// Escape a value for an XML attribute or element text.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// A small indenting XML writer.
#[derive(Debug, Default)]
struct Xml(String);

impl Xml {
    fn line(&mut self, indent: usize, text: &str) {
        for _ in 0..indent {
            self.0.push_str("  ");
        }
        self.0.push_str(text);
        self.0.push('\n');
    }
}

/// Whether a claim is fixed, for callers that report class conformance.
#[must_use]
pub const fn is_fixed(claim: &MemoryClaim) -> bool {
    !claim.balloon && claim.min_mib == claim.max_mib
}
