// mDNS-SD service registration and peer discovery.
//
// Registers this Pianeer instance as `_pianeer._tcp.local.` on `port` and
// continuously browses for other instances on the LAN.  The returned Arc is
// updated in-place whenever peers are found or lost; the UI just reads it on
// each repaint.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};

/// Shared list of discovered peers: (display label, ws:// URL).
pub type PeerList = Arc<Mutex<Vec<(String, String)>>>;

/// Start mDNS registration and browsing in a background thread.
/// Returns a shared list that is updated as peers come and go.
pub fn start(port: u16) -> PeerList {
    let peers: PeerList = Arc::new(Mutex::new(Vec::new()));
    let peers2 = Arc::clone(&peers);
    std::thread::Builder::new()
        .name("mdns".into())
        .spawn(move || run(peers2, port))
        .ok();
    peers
}

// ── Implementation ──────────────────────────────────────────────────────────

fn lan_ipv4() -> Option<Ipv4Addr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("1.1.1.1:80").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v4) => Some(v4),
        _ => None,
    }
}

fn system_hostname() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let h = h.trim().to_string();
        if !h.is_empty() { return h; }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pianeer".to_string())
}

fn run(peers: PeerList, port: u16) {
    use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

    let my_ip = match lan_ipv4() {
        Some(ip) => ip,
        None => { eprintln!("mDNS: no LAN IPv4 address, discovery disabled"); return; }
    };

    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => { eprintln!("mDNS daemon: {e}"); return; }
    };

    // Register ourselves.
    let hostname  = system_hostname();
    let host_fqdn = format!("{}.local.", hostname);
    if let Ok(info) = ServiceInfo::new(
        "_pianeer._tcp.local.",
        "Pianeer",
        &host_fqdn,
        &my_ip.to_string(),
        port,
        None::<HashMap<String, String>>,
    ) {
        if let Err(e) = daemon.register(info) {
            eprintln!("mDNS register: {e}");
        }
    }

    // Browse.
    let receiver = match daemon.browse("_pianeer._tcp.local.") {
        Ok(r) => r,
        Err(e) => { eprintln!("mDNS browse: {e}"); return; }
    };

    // Map fullname → (label, ws_url) to handle ServiceRemoved cleanly.
    let mut known: HashMap<String, (String, String)> = HashMap::new();

    while let Ok(event) = receiver.recv() {
        match event {
            ServiceEvent::ServiceResolved(info) => {
                let fullname = info.get_fullname().to_string();

                // Skip our own registration (same IP and port).
                let is_self = info.get_addresses().contains(&IpAddr::V4(my_ip))
                    && info.get_port() == port;
                if is_self { continue; }

                // Prefer IPv4 address for the ws:// URL.
                let ip = info.get_addresses().iter()
                    .find(|a| a.is_ipv4())
                    .or_else(|| info.get_addresses().iter().next())
                    .copied();

                if let Some(ip) = ip {
                    // Derive a human-readable label from the mDNS hostname.
                    let label = info.get_hostname()
                        .trim_end_matches('.')
                        .trim_end_matches(".local")
                        .to_string();
                    let label = if label.is_empty() || label == "pianeer" {
                        format!("{}", ip)
                    } else {
                        label
                    };
                    let ws_url = format!("ws://{}:{}/ws", ip, info.get_port());
                    known.insert(fullname, (label, ws_url));
                    *peers.lock().unwrap() = known.values().cloned().collect();
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                if known.remove(&fullname).is_some() {
                    *peers.lock().unwrap() = known.values().cloned().collect();
                }
            }
            _ => {}
        }
    }
}
