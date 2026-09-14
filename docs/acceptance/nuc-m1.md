# NUC Milestone One Acceptance

## Target

- Intel NUC 12 Pro `RNUC12WSHI70000`, Core i7-1260P.
- One directly connected supported USB keyboard and one flashed Relay OS USB drive.
- Secure Boot disabled before booting the image.

## Task 5 Runtime Gate Procedure

1. Build `target/relay-os.img` with `cargo xtask image --output target/relay-os.img`.
2. Record the USB device identity and SHA-256 of the exact flashed image.
3. Flash that image using an explicit host disk-imaging tool and safely eject it.
4. Enter NUC firmware setup, disable Secure Boot, and record the firmware version and changed setting.
5. Boot the USB drive with no serial terminal attached.
6. Photograph the visible framebuffer banner containing both lines below, preserving enough context to identify the NUC and boot media:

   ```text
   [relay] phase=kernel-entry status=ok
   [relay] phase=kernel-runtime status=ok
   ```

7. Save the photo hash, firmware version, image hash, date, and operator in the evidence table.
8. Do not begin Task 11 xHCI work until this physical gate is recorded as passed.

## Physical Acceptance

The clean production image completed the Task 5 NUC runtime gate. The captured
banner shows both required kernel status lines. The source photo is retained in
the separate `relay-os-artifacts` repository so this source repository contains
text evidence only.

## Evidence

| Field | Value |
| --- | --- |
| Status | Passed: clean production image displayed both required kernel status lines on the target NUC. |
| Firmware version | `WSADL357.0090.2023.0821.1714` |
| Secure Boot state | Disabled |
| USB device identity | Kingston DataTraveler 3.0, 14.40 GB, serial `0B82670162E7` |
| Flashed image SHA-256 | `b4c876089aba89510e484412fab75c94ceba072c9fc24816cf4d2b6b30b1418d` |
| Banner photo and SHA-256 | `relay-os-artifacts/evidence/20260911-020152-aade158c-20260911_085103.jpg` at artifact commit `2eb630a`; SHA-256 `5a9af08e6affb907bdab7da210723f42ca731649e096cb879bf6bcf5744b32c6` |
| Operator and date | `maw629`, 2026-09-11 |

The banner photo visibly contains both required Task 5 lines:

```text
[relay] phase=kernel-entry status=ok
[relay] phase=kernel-runtime status=ok
```

## Task 11 Platform Probe

QEMU-observed values come from `target/qemu/serial.log` after
`cargo xtask qemu boot target/relay-os.img --display none --accel tcg`
with `-device qemu-xhci,p2=2,p3=2`. Full marker line:

```text
[relay] phase=platform-probe status=ok mcfg_base=0xe0000000 bus=0-255 xhci=00:03.0 bar_base=0xc000000000 bar_size=0x4000 bar64=1 slots=64 ports=4 ctx64=0 addr64=1 scratch=0 legacy=0 usb2_off=3 usb2_count=2 usb3_off=1 usb3_count=2 dmar=0
```

| Field | QEMU observed | NUC observed (2026-09-14 physical probe) |
| --- | --- | --- |
| MCFG base | `0xe0000000` | `0xc0000000` |
| MCFG bus range | `0-255` | `0-255` |
| xHCI BDF | `00:03.0` | primary `00:14.0` (candidates `00:0d.0` + `00:14.0`) |
| BAR width/address/size | 64-bit / `0xc000000000` / `0x4000` | primary 64-bit / `0x603d180000` / `0x10000`; secondary 64-bit / `0x603d190000` / `0x10000` |
| Context size / addr width | 32-byte contexts (`ctx64=0`) / 64-bit capable (`addr64=1`) | 32-byte contexts (`ctx64=0`) / 64-bit capable (`addr64=1`) |
| Scratchpad count | `0` | `128` |
| Legacy ownership bits | `0` (no USB-legacy extended capability) | `0` |
| USB2/USB3 protocol ranges | USB2 `3:2` / USB3 `1:2` (see note) | primary USB2 `1:12` / USB3 `13:4`; secondary USB2 `1:1` / USB3 `2:3` (see NUC cross-check note) |
| VT-d firmware state + DMAR | `dmar=0` (no DMAR table under QEMU/OVMF) | `dmar=1` (VT-d Enabled in firmware) |

NUC physical probe evidence (2026-09-14 boot, operator `maw629`):
flashed image SHA-256 `9048d0e6d44ce261395cdd9f60b5f7238f6f3d8194f20acbf1740ca3b6c83513`;
banner photo `relay-os-artifacts/evidence/20260914-062324-921aabd9-20260914_132232.jpg`
SHA-256 `407091bde19a515637155425c26928276d4910ceaf71af60c2ac099fb4d49dfa`.

NUC cross-check note: the primary ranges match Linux `usb3`/`usb4`
(12+4 ports) and the secondary ranges match Linux `usb1`/`usb2` (1+3
ports); keyboard + DataTraveler verified behind `00:14.0`.

Note on the USB2/USB3 ranges: QEMU numbers the USB3 ports first
(PORTSC offsets 1-2 are USB3, offsets 3-4 are USB2), so the Supported
Protocol capabilities report USB2 `off=3 count=2` and USB3 `off=1
count=2`. Together they cover all four ports with the configured
`p2=2,p3=2` counts and no overlap, which is the topology cross-check.
An earlier draft of this table expected USB2 `1:2` / USB3 `3:2`
(USB2-first ordering); the live QEMU ext-cap dump plus the QEMU
`qemu-xhci` model source show USB3-first is what the hardware reports,
so the expectation was corrected to match the device.

Byte-position field legend: `usb2_off`/`usb2_count` and
`usb3_off`/`usb3_count` come straight from the Supported Protocol
capability dwords. Do not proceed to BOT storage work on the NUC until
this table's NUC column is filled. If VT-d translation blocks DMA
there, stop and get explicit design approval before adding an
identity-mapped DMA domain or requiring VT-d disabled in firmware.

## Linux Topology Basis For PCH Preference

Hardware-verified on the NUC via Linux, recorded here as the basis for
the `prefer_pch_primary` rule (PCH `00:14.0` preferred over lower-BDF
`00:0d.0`; Task 13 enumeration is the backstop):

- `lspci` reports two xHCI controllers: `00:0d.0` Thunderbolt
  (`8086:461e`) and `00:14.0` PCH (`8086:51ed`).
- Sysfs bus->PCI mapping: `usb1`/`usb2` -> `00:0d.0` (empty buses, no
  attached devices); `usb3`/`usb4` -> `00:14.0`.
- `lsusb -t` shows the keyboard and the DataTraveler 3.0 flash drive
  attached under the `00:14.0` buses; the `00:0d.0` buses are empty.
- Consequence: lowest-BDF-first would select the empty Thunderbolt
  controller, so Task 11 prefers bus 0, device `0x14`, function 0 when
  present and falls back to lowest BDF otherwise.

## Task 12 Controller Init (Task 3)

QEMU-observed values come from `target/qemu/serial.log` after
`cargo xtask qemu boot target/relay-os.img --display none --accel tcg`
with `-device qemu-xhci,p2=2,p3=2`. Full marker line:

```text
[relay] phase=xhci-probe status=ok slots_en=32 ports=4 ctx64=0 addr64=1 scratch=0 control_probe=none xecp=0x8 max_slots=64
```

| Field | QEMU (Task 3) | NUC (Task 4 pending) |
| --- | --- | --- |
| slots_en | `32` | Pending (Task 4) |
| ports | `4` | Pending (Task 4) |
| ctx64 | `0` | Pending (Task 4) |
| addr64 | `1` | Pending (Task 4) |
| scratch | `0` | Pending (Task 4) |
| xecp | `0x8` | Pending (Task 4) |
| max_slots | `64` | Pending (Task 4) |
| control_probe | `none` | Pending (Task 4) |
