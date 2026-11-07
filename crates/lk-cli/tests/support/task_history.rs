pub fn record(
    log: &lk_task::TransitionLog,
    transitions: &[lk_task::Transition],
    zone: &jiff::tz::TimeZone,
) -> Result<(), lk_task::TaskError> {
    for update in log.plan_record(transitions, zone)? {
        std::fs::create_dir_all(update.path.parent().unwrap()).unwrap();
        std::fs::write(update.path, update.after).unwrap();
    }
    Ok(())
}
