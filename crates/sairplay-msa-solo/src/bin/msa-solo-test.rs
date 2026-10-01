//! Hardware harness for the independent MSA SOLO engine.
#[cfg(not(windows))]
fn main() { eprintln!("msa-solo-test requires Windows WASAPI"); std::process::exit(1); }

#[cfg(windows)]
fn main() {
    if let Err(e) = run() { eprintln!("ERROR: {e}"); std::process::exit(1); }
}

#[cfg(windows)]
fn run() -> Result<(), String> {
    use sairplay_msa_solo::{WindowsMsaSoloClient, WindowsMsaSoloConfig, Ap2AudioFormat};
    use sairplay_msa_solo::route::ProtocolPreference;
    use std::{io::{self, BufRead}, sync::mpsc, thread, time::{Duration, SystemTime, UNIX_EPOCH}};
    let mut args = std::env::args().skip(1);
    let host = args.next().ok_or("Usage: msa-solo-test HOST [PORT] [auto|ap2|raop|compat] [44100|48000] [16|24]. See MSA-SOLO-HARDWARE.md")?;
    let port: u16 = args.next().unwrap_or_else(|| "7000".into()).parse().map_err(|_| "invalid port")?;
    let protocol = match args.next().as_deref().unwrap_or("auto") {
        "auto" => ProtocolPreference::Auto, "ap2" => ProtocolPreference::AirPlay2,
        "raop" => ProtocolPreference::Raop, "compat" => ProtocolPreference::AirPlay2Compat,
        _ => return Err("invalid protocol".into()),
    };
    let rate: u32 = args.next().unwrap_or_else(|| "44100".into()).parse().map_err(|_| "invalid sample rate")?;
    let bits: u16 = args.next().unwrap_or_else(|| "16".into()).parse().map_err(|_| "invalid bit depth")?;
    if !matches!(rate, 44100 | 48000) || !matches!(bits, 16 | 24) || args.next().is_some() {
        return Err("expected 44100/48000 Hz and 16/24 bits; too many arguments are rejected".into());
    }
    let mut config = WindowsMsaSoloConfig::new(host, port, port);
    config.protocol = protocol;
    config.txt = std::env::var("MSA_TEST_TXT").ok();
    config.am = std::env::var("MSA_TEST_MODEL").ok();
    config.native.control.audio_format = Ap2AudioFormat { sample_rate: rate, bit_depth: bits, channels: 2 };
    config.native.control.auth_credentials = std::env::var("MSA_TEST_CREDENTIALS").ok();
    config.native.control.password = std::env::var("MSA_TEST_PASSWORD").ok();
    config.raop.password = config.native.control.password.clone();
    config.raop.secret = std::env::var("MSA_TEST_RAOP_SECRET").ok();
    config.raop_cn = std::env::var("MSA_TEST_CN").ok();
    config.raop_pk = std::env::var("MSA_TEST_PK").ok();
    config.pw_txt = std::env::var("MSA_TEST_PW").ok();
    if let Ok(value) = std::env::var("MSA_TEST_TIMING") {
        config.ptp_override = Some(match value.as_str() {
            "ptp" => true, "ntp" => false, _ => return Err("MSA_TEST_TIMING must be ptp or ntp".into()),
        });
    }
    config.buffered_forced = match std::env::var("MSA_TEST_BUFFERED").as_deref() {
        Ok("1") => true, Ok("0") | Err(_) => false, _ => return Err("MSA_TEST_BUFFERED must be 0 or 1".into()),
    };
    let now = || SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    eprintln!("CONNECT requested={rate}/{bits}; credentials are never printed");
    let mut client = WindowsMsaSoloClient::connect(config)
        .map_err(|e| format!("CONNECT class={:?} status={} route={:?}", e.class, e.http_status, e.route))?;
    let result = (|| -> Result<(), String> {
        eprintln!("READY route={:?} capabilities={:?} latency={:?} ptp={}", client.route(), client.format_capabilities(), client.latency_info(), client.uses_ptp());
        client.set_volume(50).map_err(|e| format!("volume: {e:?}"))?;
        client.set_metadata("MSA SOLO hardware test", "SolYan", "Independent engine", 0, "hardware-test")
            .map_err(|e| format!("metadata: {e:?}"))?;
        eprintln!("START {:?}", client.commit_start(now()).map_err(|e| format!("{e:?}"))?);
        eprintln!("Play music on this Windows PC. Commands: pause, play, flush, standby, start, stop, progress ELAPSED DURATION, artwork PATH, quit");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                match line { Ok(line) => { if tx.send(line).is_err() { break; } }, Err(_) => break }
            }
        });
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(line) => {
                    let mut words = line.split_whitespace();
                    let command = words.next().unwrap_or("");
                    let outcome: Result<(), String> = match command {
                        "quit" => break,
                        "pause" => client.pause_content().map_err(|e| format!("{e:?}")),
                        "play" => client.play_content().map_err(|e| format!("{e:?}")),
                        "stop" => client.stop_content().map_err(|e| format!("{e:?}")),
                        "standby" => client.standby_content().map_err(|e| format!("{e:?}")),
                        "start" => client.commit_start(now()).map(|r| eprintln!("START {r:?}")).map_err(|e| format!("{e:?}")),
                        "flush" => client.flush_content().map_err(|e| format!("{e:?}")).and_then(|ack| {
                            eprintln!("FLUSH {ack:?}");
                            client.commit_start(now()).map(|r| eprintln!("START {r:?}")).map_err(|e| format!("{e:?}"))
                        }),
                        "progress" => match (words.next().and_then(|v| v.parse().ok()), words.next().and_then(|v| v.parse().ok())) {
                            (Some(elapsed), Some(duration)) => client.set_progress(elapsed, duration).map_err(|e| format!("{e:?}")),
                            _ => Err("progress ELAPSED_SECONDS DURATION_SECONDS".into()),
                        },
                        "artwork" => {
                            let path = line.trim().strip_prefix("artwork").unwrap_or("").trim();
                            std::fs::read(path).map_err(|e| e.to_string()).and_then(|bytes| {
                                let mime = if bytes.starts_with(&[0xff, 0xd8]) { "image/jpeg" } else { "image/png" };
                                client.set_artwork(mime, &bytes).map_err(|e| format!("{e:?}"))
                            })
                        },
                        "" => Ok(()),
                        _ => Err("unknown command".into()),
                    };
                    eprintln!("COMMAND {command}: {outcome:?}");
                },
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {},
            }
            while let Some(command) = client.pop_remote_command() { eprintln!("REMOTE {command:?}"); }
            eprintln!("STATUS state={:?} connected={} playing={} healthy={} head={} clock={:?} verify={:?} mrp={} media={:?}",
                client.state(), client.is_connected(), client.is_playing(), client.control_healthy(),
                client.head_audible_unix_ms(), client.clock_readiness(), client.poll_clock_verify(),
                client.mrp_channel_status(), client.diagnostics());
            if !client.control_healthy() { return Err("session lost; inspect STATUS before failure".into()); }
        }
        Ok(())
    })();
    let teardown = client.disconnect().map_err(|e| format!("TEARDOWN {e:?}"));
    eprintln!("TEARDOWN {teardown:?}; no hardware PASS is inferred from counters");
    result.and(teardown)
}
