//! Domain rendering: the desktop class and the dev class.

use cdrv::{
    BlockDevice, CpuClaim, DeviceAddress, GuestOs, InstanceClass, InstanceId, InstanceKind,
    InstanceSpec, MemoryClaim, Nic, SectorSize,
};
use clnx::{RenderOptions, render_domain};

fn disk(path: &str, serial: Option<&str>) -> BlockDevice {
    BlockDevice {
        path: path.into(),
        serial: serial.map(str::to_owned),
        sector: SectorSize::FOUR_KN,
        read_only: false,
    }
}

fn card(base: u8) -> Vec<DeviceAddress> {
    (0..4)
        .map(|function| DeviceAddress {
            domain: 0,
            bus: base,
            slot: 0,
            function,
        })
        .collect()
}

fn desktop_windows() -> InstanceSpec {
    InstanceSpec {
        id: InstanceId::from("desktop-win"),
        kind: InstanceKind::Vm,
        guest_os: GuestOs::Windows,
        memory: MemoryClaim::fixed(24 * 1024),
        cpu: CpuClaim { quota_cores: 8.0 },
        class: InstanceClass::Desktop,
        devices: card(0x03),
        disks: vec![
            disk("/dev/zvol/pool/vms/desktop-win", Some("SYNTHETIC-DISK-1")),
            disk("/dev/zvol/pool/vms/desktop-win-data", None),
        ],
        vcpus: Some(8),
        nic: Some(Nic {
            mac: "52:54:00:00:00:01".to_owned(),
            model: Some("virtio".to_owned()),
        }),
        console: false,
        identity: None,
    }
}

fn options_windows() -> RenderOptions {
    RenderOptions {
        mmio64_mib: 65_536,
        emulated_display: false,
        ..RenderOptions::default()
    }
}

fn dev_macos() -> InstanceSpec {
    InstanceSpec {
        id: InstanceId::from("dev-mac"),
        kind: InstanceKind::Vm,
        guest_os: GuestOs::MacOs,
        memory: MemoryClaim {
            min_mib: 4 * 1024,
            max_mib: 8 * 1024,
            balloon: true,
        },
        cpu: CpuClaim { quota_cores: 4.0 },
        class: InstanceClass::Dev,
        devices: Vec::new(),
        disks: vec![disk("/dev/zvol/pool/vms/dev-mac", Some("SYNTHETIC-DISK-2"))],
        vcpus: Some(4),
        nic: None,
        console: false,
        identity: None,
    }
}

fn options_macos() -> RenderOptions {
    RenderOptions {
        opencore_iso: Some("/images/opencore.iso".into()),
        apple_smc_osk: Some("test-osk".to_owned()),
        cpu_model: Some("Skylake-Client-v4,vendor=GenuineIntel,+invtsc,kvm=on".to_owned()),
        ..RenderOptions::default()
    }
}

#[test]
fn desktop_windows_has_vfio_the_rebar_window_and_no_balloon() {
    let xml = render_domain(&desktop_windows(), &options_windows()).unwrap();

    assert_eq!(
        xml.matches("<hostdev mode='subsystem' type='pci' managed='no'>")
            .count(),
        4
    );
    assert_eq!(xml.matches("<driver name='vfio'/>").count(), 4);
    assert!(xml.contains("<source>"));
    assert!(xml.contains("<address domain='0x0000' bus='0x03' slot='0x00' function='0x3'/>"));
    assert!(xml.contains(
        "<address type='pci' domain='0x0000' bus='0x01' slot='0x00' function='0x0' multifunction='on'/>"
    ));
    assert!(xml.contains("opt/ovmf/X-PciMmio64Mb,string=65536"));
    assert!(xml.contains("<memballoon model='none'/>"));
    assert!(!xml.contains("freePageReporting"));
    assert!(!xml.contains("<memballoon model='virtio'"));

    // Both disks are 4Kn and are virtio disks on the guest.
    assert_eq!(
        xml.matches("<blockio logical_block_size='4096' physical_block_size='4096'/>")
            .count(),
        2
    );
    assert!(xml.contains("<target dev='vda' bus='virtio'/>"));
    assert!(xml.contains("<target dev='vdb' bus='virtio'/>"));
    assert!(xml.contains("<serial>SYNTHETIC-DISK-1</serial>"));
    assert_eq!(xml.matches("<hyperv mode='custom'>").count(), 1);
    assert!(xml.contains("<tpm model='tpm-tis'>"));
}

#[test]
fn dev_macos_puts_the_balloon_on_pcie_zero_and_has_no_rebar_window() {
    let xml = render_domain(&dev_macos(), &options_macos()).unwrap();

    let balloon = xml
        .split_once("<memballoon ")
        .and_then(|(_, rest)| rest.split_once("</memballoon>"))
        .map(|(block, _)| block)
        .expect("a balloon");
    assert!(balloon.contains("model='virtio'"));
    assert!(balloon.contains("freePageReporting='on'"));
    assert!(
        balloon.contains(
            "<address type='pci' domain='0x0000' bus='0x00' slot='0x13' function='0x0'/>"
        ),
        "the macOS balloon must sit on pcie.0, got {balloon}"
    );
    assert!(!xml.contains("X-PciMmio64Mb"));
    assert!(!xml.contains("<hostdev"));
    assert_eq!(
        xml.matches("<blockio logical_block_size='4096' physical_block_size='4096'/>")
            .count(),
        1
    );
    assert!(xml.contains("<disk type='file' device='cdrom'>"));
    assert!(xml.contains("<target dev='sdb' bus='sata' rotation_rate='1'/>"));
    assert!(xml.contains("isa-applesmc,osk=test-osk"));
    assert!(xml.contains("<graphics") || xml.contains("<video>"));
}

#[test]
fn a_512e_disk_is_refused() {
    let mut spec = desktop_windows();
    spec.disks[0].sector = SectorSize {
        logical: 512,
        physical: 4096,
    };
    let error = render_domain(&spec, &options_windows()).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("4Kn always"), "{text}");
}

#[test]
fn a_balloon_on_a_pinned_claim_is_refused() {
    let mut spec = desktop_windows();
    spec.memory = MemoryClaim {
        min_mib: 8 * 1024,
        max_mib: 24 * 1024,
        balloon: true,
    };
    let error = render_domain(&spec, &options_windows()).unwrap_err();
    assert!(error.to_string().contains("cannot balloon"));
}

#[test]
fn a_fixed_claim_must_really_be_fixed() {
    let mut spec = desktop_windows();
    spec.memory = MemoryClaim {
        min_mib: 8 * 1024,
        max_mib: 24 * 1024,
        balloon: false,
    };
    let error = render_domain(&spec, &options_windows()).unwrap_err();
    assert!(error.to_string().contains("min_mib == max_mib"));
}

#[test]
fn containers_are_not_libvirt_domains() {
    let mut spec = desktop_windows();
    spec.kind = InstanceKind::Container;
    let error = render_domain(&spec, &options_windows()).unwrap_err();
    assert!(error.to_string().contains("unsupported"));
}

#[test]
fn the_rebar_window_needs_a_desktop_with_devices() {
    let mut spec = desktop_windows();
    spec.class = InstanceClass::Dev;
    spec.devices.clear();
    let xml = render_domain(&spec, &options_windows()).unwrap();
    assert!(!xml.contains("X-PciMmio64Mb"));
    assert!(!xml.contains("<hostdev"));
}

#[test]
fn the_rendered_domains_match_their_snapshots() {
    assert_eq!(
        render_domain(&desktop_windows(), &options_windows()).unwrap(),
        include_str!("fixtures/desktop-windows.xml")
    );
    assert_eq!(
        render_domain(&dev_macos(), &options_macos()).unwrap(),
        include_str!("fixtures/dev-macos.xml")
    );
}

#[test]
fn spec_console_identity_and_vcpus_reach_the_xml() {
    let mut spec = desktop_windows();
    spec.console = true;
    spec.vcpus = Some(6);
    spec.identity = Some(cdrv::Smbios {
        manufacturer: "Synthetic Maker".to_owned(),
        product: "Synthetic Product".to_owned(),
        version: "1".to_owned(),
        system_uuid: None,
        board_manufacturer: None,
        board_product: None,
        board_version: None,
    });
    let xml = render_domain(&spec, &options_windows()).unwrap();
    assert!(xml.contains("<vcpu placement='static'>6</vcpu>"));
    assert!(xml.contains("<listen type='socket'/>"));
    assert!(xml.contains("Synthetic Maker"));
    assert!(xml.contains("<smbios mode='sysinfo'/>"));
}

#[test]
fn zero_vcpus_are_refused() {
    let mut spec = dev_macos();
    spec.vcpus = Some(0);
    assert!(render_domain(&spec, &options_macos()).is_err());
}
