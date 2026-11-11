use anyhow::Result;

fn main() -> Result<()> {
    let sessions = dsh_sdk::sessions::scan_sessions()?;
    println!("{} sessions", sessions.len());
    for session in sessions.iter().take(15) {
        let events = dsh_sdk::sessions::load_events(&session.path)?.len();
        println!(
            "{}  cwd={}  blank={}  title={:?}  events={}",
            session.id, session.cwd, session.blank, session.title, events
        );
    }
    Ok(())
}
