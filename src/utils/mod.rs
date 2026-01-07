use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub fn fetch_public_ip() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(Duration::from_secs(10)))
                .build(),
        );
        let res = agent
            .get("https://api.ipify.org")
            .call()
            .and_then(|r| Ok(r.into_body().read_to_string().unwrap_or_default()));
        let _ = tx.send(res.unwrap_or_else(|_| "Fetch Failed".into()));
    });
    rx
}

pub fn apply_affinity(index: Option<usize>) {
    if let Some(i) = index {
        if let Some(ids) = core_affinity::get_core_ids() {
            if i < ids.len() {
                core_affinity::set_for_current(ids[i]);
            }
        }
    }
}
