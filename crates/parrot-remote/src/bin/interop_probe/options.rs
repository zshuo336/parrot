use std::time::Duration;
pub struct Options {
    pub port: u16,
    pub startup: Duration,
    pub business: Duration,
}
impl Options {
    pub fn parse(startup: Duration, business: Duration) -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let mut options = Self {
            port: 0,
            startup,
            business,
        };
        let mut port_seen = false;
        let mut startup_seen = false;
        let mut business_seen = false;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--startup-timeout-ms" | "--business-timeout-ms" => {
                    let seen = if arg == "--startup-timeout-ms" {
                        &mut startup_seen
                    } else {
                        &mut business_seen
                    };
                    if *seen {
                        return Err(format!("duplicate option: {arg}"));
                    }
                    *seen = true;
                    let text = args
                        .next()
                        .ok_or_else(|| format!("missing value for {arg}"))?;
                    let ms = decimal(&text)?.parse::<u64>().map_err(|e| e.to_string())?;
                    if !(1..=600000).contains(&ms) {
                        return Err(format!("{arg} must be between 1 and 600000"));
                    }
                    if arg == "--startup-timeout-ms" {
                        options.startup = Duration::from_millis(ms);
                    } else {
                        options.business = Duration::from_millis(ms);
                    }
                }
                _ if !port_seen => {
                    options.port = decimal(&arg)?
                        .parse()
                        .map_err(|e| format!("invalid port: {e}"))?;
                    port_seen = true;
                }
                _ => return Err(format!("unexpected argument: {arg}")),
            }
        }
        Ok(options)
    }
}
fn decimal(value: &str) -> Result<&str, String> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        Err(format!("expected decimal integer: {value}"))
    } else {
        Ok(value)
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Signals {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .map_err(|e| e.to_string())?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|e| e.to_string())?,
        })
    }
    pub async fn cancelled(&mut self) {
        tokio::select! { _ = self.interrupt.recv() => {}, _ = self.terminate.recv() => {} }
    }
}
