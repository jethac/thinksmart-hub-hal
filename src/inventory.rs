//! A one-shot description of the hardware, from /sys and /proc plus `ip` and
//! `iw`. Cheap enough to regenerate on demand; never polled.

use crate::led::{hidraw_for, hidraw_interface};
use crate::util::{read_trim, run};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::time::Duration;

/// USB devices this hardware is known to carry, with what they are for.
const KNOWN_USB: &[(&str, &str, &str)] = &[
    ("17ef", "a017", "Speakers + mic array + LED ring"),
    ("17ef", "60c0", "PIR proximity sensor"),
    ("17ef", "60ce", "Unidentified Lenovo HID"),
];

fn entries(dir: &str) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

pub fn collect(ddc_bus: Option<u32>) -> Value {
    let usb = usb();
    let input = input_devices();
    let video = video();
    let net = network();
    let bt: Vec<String> = entries("/sys/class/bluetooth").iter().map(|p| name_of(p)).collect();
    let prox_iio = entries("/sys/bus/iio/devices")
        .iter()
        .any(|p| read_trim(p.join("name")).as_deref() == Some("prox"));
    let has_usb = |pid: &str| usb.iter().any(|d| d["vid"] == "17ef" && d["pid"] == pid);
    let ifaces = net["interfaces"].as_array().cloned().unwrap_or_default();
    let has_iface = |wireless: bool| ifaces.iter().any(|i| i["wireless"] == wireless && i["name"] != "lo");
    let touch = input.iter().any(|d| d["name"].as_str().is_some_and(|n| n.contains("SYNA7508")));

    let check = |id: &str, label: &str, ok: bool, detail: &str| json!({"id": id, "label": label, "ok": ok, "detail": detail});
    let checks = vec![
        check("ddc", "Panel DDC/CI", ddc_bus.is_some(), &ddc_bus.map(|b| format!("/dev/i2c-{b}")).unwrap_or_default()),
        check("touch", "Touch controller", touch, "SYNA7508 over I2C-HID"),
        check("prox", "Proximity (IIO)", prox_iio, "hid-sensor-prox"),
        check("audio", "Audio device", has_usb("a017"), "17ef:a017"),
        check("led", "LED ring node", !hidraw_for("17EF", "A017").is_empty(), "hidraw on 17ef:a017"),
        check("pir-usb", "Sensor hub", has_usb("60c0"), "17ef:60c0"),
        check("hdmi-in", "HDMI-in capture", !video.is_empty(), "UVC /dev/video*"),
        check("wifi", "Wi-Fi", has_iface(true), "iwlwifi"),
        check("ethernet", "Ethernet", has_iface(false), ""),
        check("bt", "Bluetooth", !bt.is_empty(), "hci"),
        check("tpm", "TPM", Path::new("/sys/class/tpm/tpm0").exists(), ""),
    ];

    json!({
        "system": system(),
        "thermal": thermal(),
        "storage": storage(),
        "usb": usb,
        "hidraw": hidraw(),
        "input": input,
        "video": video,
        "network": net,
        "bluetooth": bt,
        "rfkill": rfkill(),
        "checks": checks,
    })
}

fn system() -> Value {
    let dmi = |f: &str| read_trim(format!("/sys/class/dmi/id/{f}"));
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpu = cpuinfo
        .lines()
        .find_map(|l| l.strip_prefix("model name").and_then(|r| r.split_once(':')).map(|(_, v)| v.trim().to_string()));
    let cpus = cpuinfo.lines().filter(|l| l.starts_with("processor")).count();
    let mem_gb = fs::read_to_string("/proc/meminfo").ok().and_then(|m| {
        m.lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|v| v.split_whitespace().next()?.parse::<f64>().ok())
            .map(|kb| (kb / 1_048_576.0 * 10.0).round() / 10.0)
    });
    let os = fs::read_to_string("/etc/os-release").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
    });
    let uptime = read_trim("/proc/uptime")
        .and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok())
        .map(|s| s as u64);
    json!({
        "hostname": read_trim("/proc/sys/kernel/hostname"),
        "vendor": dmi("sys_vendor"),
        "machine_type": dmi("product_name"),
        "model": dmi("product_version"),
        "bios": dmi("bios_version"),
        "bios_date": dmi("bios_date"),
        "os": os,
        "kernel": read_trim("/proc/sys/kernel/osrelease"),
        "cpu": cpu,
        "cpus": cpus,
        "mem_gb": mem_gb,
        "uptime_s": uptime,
        "tpm_version": read_trim("/sys/class/tpm/tpm0/tpm_version_major"),
    })
}

fn thermal() -> Value {
    let chips: Vec<Value> = entries("/sys/class/hwmon")
        .iter()
        .map(|dir| {
            let mut sensors = vec![];
            for f in entries(&dir.to_string_lossy()) {
                let n = name_of(&f);
                let (kind, unit, div) = if n.starts_with("temp") && n.ends_with("_input") {
                    ("temp", "°C", 1000.0)
                } else if n.starts_with("fan") && n.ends_with("_input") {
                    ("fan", "rpm", 1.0)
                } else {
                    continue;
                };
                let base = n.trim_end_matches("_input");
                let label = read_trim(dir.join(format!("{base}_label"))).unwrap_or_else(|| base.to_string());
                if let Some(v) = read_trim(&f).and_then(|v| v.parse::<f64>().ok()) {
                    sensors.push(json!({"kind": kind, "label": label, "value": v / div, "unit": unit}));
                }
            }
            json!({"chip": read_trim(dir.join("name")), "sensors": sensors})
        })
        .collect();
    Value::Array(chips)
}

fn storage() -> Value {
    let disks: Vec<Value> = entries("/sys/block")
        .iter()
        .filter(|p| {
            let n = name_of(p);
            !(n.starts_with("loop") || n.starts_with("ram") || n.starts_with("zram"))
        })
        .map(|p| {
            let gb = read_trim(p.join("size"))
                .and_then(|s| s.parse::<f64>().ok())
                .map(|sectors| (sectors * 512.0 / 1e9).round());
            json!({"name": name_of(p), "model": read_trim(p.join("device/model")), "size_gb": gb})
        })
        .collect();
    Value::Array(disks)
}

fn usb() -> Vec<Value> {
    entries("/sys/bus/usb/devices")
        .iter()
        .filter_map(|p| {
            let vid = read_trim(p.join("idVendor"))?;
            let pid = read_trim(p.join("idProduct"))?;
            let role = KNOWN_USB.iter().find(|(v, d, _)| *v == vid && *d == pid).map(|(_, _, r)| *r);
            Some(json!({
                "port": name_of(p),
                "vid": vid,
                "pid": pid,
                "manufacturer": read_trim(p.join("manufacturer")),
                "product": read_trim(p.join("product")),
                "speed_mbps": read_trim(p.join("speed")),
                "role": role,
            }))
        })
        .collect()
}

fn hidraw() -> Value {
    let nodes: Vec<Value> = entries("/sys/class/hidraw")
        .iter()
        .map(|p| {
            let n = name_of(p);
            let ue = fs::read_to_string(p.join("device/uevent")).unwrap_or_default();
            let get = |k: &str| ue.lines().find_map(|l| l.strip_prefix(k)).map(str::to_string);
            let id = get("HID_ID=").unwrap_or_default();
            let parts: Vec<&str> = id.split(':').collect();
            let (bus, vid, pid) = if parts.len() == 3 {
                let bus = match parts[0] {
                    "0003" => "usb",
                    "0018" => "i2c",
                    "0005" => "bluetooth",
                    other => other,
                };
                let tail = |s: &str| s[s.len().saturating_sub(4)..].to_lowercase();
                (bus.to_string(), tail(parts[1]), tail(parts[2]))
            } else {
                (String::new(), String::new(), String::new())
            };
            json!({
                "node": n,
                "name": get("HID_NAME="),
                "bus": bus,
                "vid": vid,
                "pid": pid,
                "interface": hidraw_interface(&n),
                "readable": fs::File::open(format!("/dev/{n}")).is_ok(),
            })
        })
        .collect();
    Value::Array(nodes)
}

fn input_devices() -> Vec<Value> {
    let text = fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
    text.split("\n\n")
        .filter(|b| !b.trim().is_empty())
        .map(|block| {
            let mut name = None;
            let mut handlers = None;
            let mut id = None;
            for l in block.lines() {
                if let Some(v) = l.strip_prefix("N: Name=") {
                    name = Some(v.trim_matches('"').to_string());
                } else if let Some(v) = l.strip_prefix("H: Handlers=") {
                    handlers = Some(v.trim().to_string());
                } else if let Some(v) = l.strip_prefix("I: ") {
                    id = Some(v.trim().to_string());
                }
            }
            json!({"name": name, "handlers": handlers, "id": id})
        })
        .collect()
}

fn video() -> Vec<Value> {
    entries("/sys/class/video4linux")
        .iter()
        .map(|p| json!({"node": name_of(p), "name": read_trim(p.join("name")), "index": read_trim(p.join("index"))}))
        .collect()
}

fn network() -> Value {
    let t = Duration::from_secs(4);
    let addrs: Vec<Value> = {
        let out = run("ip", &["-j", "addr"], t);
        serde_json::from_str(&out.text).unwrap_or_default()
    };
    let interfaces: Vec<Value> = entries("/sys/class/net")
        .iter()
        .filter(|p| name_of(p) != "lo")
        .map(|p| {
            let n = name_of(p);
            let wireless = p.join("wireless").exists() || p.join("phy80211").exists();
            let ips: Vec<String> = addrs
                .iter()
                .filter(|a| a["ifname"] == n.as_str())
                .flat_map(|a| a["addr_info"].as_array().cloned().unwrap_or_default())
                .filter_map(|ai| Some(format!("{}/{}", ai["local"].as_str()?, ai["prefixlen"])))
                .collect();
            let driver = fs::read_link(p.join("device/driver")).ok().map(|d| name_of(&d));
            let link = wireless.then(|| run("iw", &["dev", &n, "link"], t).text.trim().to_string());
            json!({
                "name": n,
                "wireless": wireless,
                "state": read_trim(p.join("operstate")),
                "mac": read_trim(p.join("address")),
                "speed_mbps": read_trim(p.join("speed")),
                "driver": driver,
                "addresses": ips,
                "wifi_link": link,
            })
        })
        .collect();
    json!({"interfaces": interfaces})
}

fn rfkill() -> Value {
    let v: Vec<Value> = entries("/sys/class/rfkill")
        .iter()
        .map(|p| {
            json!({
                "name": read_trim(p.join("name")),
                "type": read_trim(p.join("type")),
                "soft_blocked": read_trim(p.join("soft")).as_deref() == Some("1"),
                "hard_blocked": read_trim(p.join("hard")).as_deref() == Some("1"),
            })
        })
        .collect();
    Value::Array(v)
}
