//! Per-process listening port enumeration via Windows IpHelper.
//!
//! Uses `GetExtendedTcpTable` (LISTEN state only) and `GetExtendedUdpTable`
//! to enumerate which TCP/UDP ports each watched process has open.
//! Both calls do a single system-wide snapshot — no per-process API overhead.

use std::collections::HashMap;
use windows::Win32::Foundation::BOOL;
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable,
    MIB_TCPTABLE_OWNER_PID, MIB_UDPTABLE_OWNER_PID,
    TCP_TABLE_OWNER_PID_LISTENER, UDP_TABLE_OWNER_PID,
};

/// AF_INET (IPv4) — matches winsock2.h; stable constant, no WinSock feature needed.
const AF_INET: u32 = 2;

pub struct PortEntry {
    pub pid:        u32,
    pub tcp_listen: Vec<u16>,
    pub udp_listen: Vec<u16>,
}

/// Return one `PortEntry` for every pid in `known_pids` that has at least one
/// listening TCP socket or bound UDP endpoint.  Pids with no sockets are omitted.
pub fn sample_listening(known_pids: &[u32]) -> Vec<PortEntry> {
    let mut tcp_map: HashMap<u32, Vec<u16>> = HashMap::new();
    let mut udp_map: HashMap<u32, Vec<u16>> = HashMap::new();

    // ── TCP LISTEN sockets (IPv4) ─────────────────────────────────────────────
    unsafe {
        let mut size: u32 = 0;
        // First call with null buffer to obtain the required buffer size.
        GetExtendedTcpTable(
            None, &mut size, BOOL(0), AF_INET, TCP_TABLE_OWNER_PID_LISTENER, 0,
        );
        if size > 0 {
            let mut buf = vec![0u8; size as usize];
            let rc = GetExtendedTcpTable(
                Some(buf.as_mut_ptr() as *mut _),
                &mut size, BOOL(0), AF_INET,
                TCP_TABLE_OWNER_PID_LISTENER, 0,
            );
            if rc == 0 && (size as usize) >= std::mem::size_of::<u32>() {
                let table = &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
                let rows = std::slice::from_raw_parts(
                    table.table.as_ptr(),
                    table.dwNumEntries as usize,
                );
                for row in rows {
                    if known_pids.contains(&row.dwOwningPid) {
                        // dwLocalPort is in network byte order (big-endian).
                        let port = u16::from_be(row.dwLocalPort as u16);
                        tcp_map.entry(row.dwOwningPid).or_default().push(port);
                    }
                }
            }
        }
    }

    // ── UDP bound endpoints (IPv4) ────────────────────────────────────────────
    unsafe {
        let mut size: u32 = 0;
        GetExtendedUdpTable(
            None, &mut size, BOOL(0), AF_INET, UDP_TABLE_OWNER_PID, 0,
        );
        if size > 0 {
            let mut buf = vec![0u8; size as usize];
            let rc = GetExtendedUdpTable(
                Some(buf.as_mut_ptr() as *mut _),
                &mut size, BOOL(0), AF_INET,
                UDP_TABLE_OWNER_PID, 0,
            );
            if rc == 0 && (size as usize) >= std::mem::size_of::<u32>() {
                let table = &*(buf.as_ptr() as *const MIB_UDPTABLE_OWNER_PID);
                let rows = std::slice::from_raw_parts(
                    table.table.as_ptr(),
                    table.dwNumEntries as usize,
                );
                for row in rows {
                    if known_pids.contains(&row.dwOwningPid) {
                        let port = u16::from_be(row.dwLocalPort as u16);
                        udp_map.entry(row.dwOwningPid).or_default().push(port);
                    }
                }
            }
        }
    }

    // ── Combine, deduplicate, sort ────────────────────────────────────────────
    let active_pids: Vec<u32> = known_pids.iter()
        .copied()
        .filter(|pid| tcp_map.contains_key(pid) || udp_map.contains_key(pid))
        .collect();

    active_pids.into_iter()
        .map(|pid| {
            let mut tcp = tcp_map.remove(&pid).unwrap_or_default();
            let mut udp = udp_map.remove(&pid).unwrap_or_default();
            tcp.sort_unstable();
            tcp.dedup();
            udp.sort_unstable();
            udp.dedup();
            PortEntry { pid, tcp_listen: tcp, udp_listen: udp }
        })
        .collect()
}
