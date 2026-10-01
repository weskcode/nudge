// when each nudge fires next, and which ones the reminder on screen answers.
// no windows, menus or files here, so the timing rules can be tested alone

use std::collections::HashMap;
use std::time::{Duration, Instant};

// the reminder on screen wears one nudge's look and answers every nudge in
// `due`; a preview answers none, so dismissing it restarts nothing
pub struct Showing {
    pub look: u32,
    pub due: Vec<u32>,
    pub seq: u32,
    pub windows: Vec<String>,
    pub shown_at: Instant,
}

pub struct Schedule {
    // the master switch; while off no timer runs
    pub on: bool,
    // enabled nudges waiting for their time; a nudge on screen is not here
    pub next: HashMap<u32, Instant>,
    pub showing: Option<Showing>,
    // the end of a pause from the menu; a timer started before then waits for it
    paused_until: Option<Instant>,
    seq: u32,
}

impl Schedule {
    pub fn new() -> Self {
        Self {
            on: false,
            next: HashMap::new(),
            showing: None,
            paused_until: None,
            seq: 0,
        }
    }

    // a fresh number for the next reminder's windows, so a timer left over
    // from an earlier reminder can tell it is stale
    pub fn next_seq(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    fn is_due_on_screen(&self, id: u32) -> bool {
        self.showing.as_ref().is_some_and(|s| s.due.contains(&id))
    }

    // the nudges to show now, soonest first: none unless one has come due (or
    // `force`, which takes the soonest anyway), then every nudge due within
    // `window` comes along so two reminders don't open a minute apart
    pub fn due_now(&self, now: Instant, window: Duration, force: bool) -> Vec<u32> {
        let mut waiting: Vec<(Instant, u32)> = self.next.iter().map(|(&id, &at)| (at, id)).collect();
        waiting.sort();
        let Some(&(first, _)) = waiting.first() else {
            return Vec::new();
        };
        if first > now && !force {
            return Vec::new();
        }
        let until = now + window;
        waiting
            .iter()
            .enumerate()
            .filter(|&(index, &(at, _))| index == 0 || at <= until)
            .map(|(_, &(_, id))| id)
            .collect()
    }

    // a reminder opened: the nudges it answers stop waiting
    pub fn open(&mut self, showing: Showing) {
        for id in &showing.due {
            self.next.remove(id);
        }
        self.showing = Some(showing);
    }

    // nudges that came due while a reminder is up join it instead of queueing
    pub fn join(&mut self, ids: &[u32]) {
        let Some(showing) = self.showing.as_mut() else {
            return;
        };
        for &id in ids {
            if self.next.remove(&id).is_some() && !showing.due.contains(&id) {
                showing.due.push(id);
            }
        }
    }

    pub fn close(&mut self) -> Option<Showing> {
        self.showing.take()
    }

    // start these nudges' timers from now; nothing runs while reminders are
    // off, a nudge on screen restarts when it is dismissed, and none fires
    // inside a pause
    pub fn resume(&mut self, timers: &[(u32, Duration)], now: Instant) {
        if !self.on {
            return;
        }
        for &(id, after) in timers {
            if !self.is_due_on_screen(id) {
                let at = (now + after).max(self.paused_until.unwrap_or(now));
                self.next.insert(id, at);
            }
        }
    }

    // every enabled nudge starts a full interval over, ending any pause
    pub fn restart(&mut self, timers: &[(u32, Duration)], now: Instant) {
        self.next.clear();
        self.paused_until = None;
        self.resume(timers, now);
    }

    // after the Mac wakes, every enabled nudge starts a full interval over. a
    // pause carries on, less the time asleep: Instant stops during sleep, so a
    // two-hour pause with an hour asleep has an hour left
    pub fn after_wake(&mut self, timers: &[(u32, Duration)], now: Instant, slept: Duration) {
        let paused_until = self
            .paused_until
            .and_then(|until| until.checked_sub(slept))
            .filter(|&until| until > now);
        self.next.clear();
        self.paused_until = paused_until;
        self.resume(timers, now);
    }

    // after a wake that keeps the timers, a pause still loses the time asleep;
    // nudges it was holding back move with it, and if it ran out while the Mac
    // slept they start a full interval from now, as when a pause is ended
    pub fn shorten_pause(&mut self, timers: &[(u32, Duration)], slept: Duration, now: Instant) {
        let Some(old) = self.paused_until else {
            return;
        };
        let until = old.checked_sub(slept).filter(|&until| until > now);
        for (id, at) in self.next.iter_mut() {
            if *at == old {
                let full = timers.iter().find(|(t, _)| t == id).map_or(now, |&(_, after)| now + after);
                *at = until.unwrap_or(full);
            }
        }
        self.paused_until = until;
    }

    // nothing fires before `until`; a nudge already due later keeps its time
    pub fn pause(&mut self, ids: &[u32], until: Instant) {
        if !self.on {
            return;
        }
        self.paused_until = self.paused_until.max(Some(until));
        for &id in ids {
            let at = self.next.entry(id).or_insert(until);
            *at = (*at).max(until);
        }
    }

    // a break taken on purpose ends a pause, as it did with one reminder: the
    // nudges the pause was holding back start a full interval over from now
    pub fn end_pause(&mut self, timers: &[(u32, Duration)], now: Instant) {
        let Some(until) = self.paused_until.take() else {
            return;
        };
        for &(id, after) in timers {
            if self.next.get(&id) == Some(&until) {
                self.next.insert(id, now + after);
            }
        }
    }

    // a deleted nudge stops waiting and leaves the reminder on screen
    pub fn forget(&mut self, id: u32) {
        self.next.remove(&id);
        if let Some(showing) = self.showing.as_mut() {
            showing.due.retain(|&due| due != id);
        }
    }

    pub fn soonest(&self) -> Option<Instant> {
        self.next.values().min().copied()
    }

    // seconds until a nudge fires: 0 while it is on screen, -1 when it has no timer
    pub fn remaining(&self, id: u32, now: Instant) -> i64 {
        if self.is_due_on_screen(id) {
            0
        } else if let Some(at) = self.next.get(&id) {
            at.saturating_duration_since(now).as_secs() as i64
        } else {
            -1
        }
    }

    // the countdown lines at the top of the tray menu. one nudge reads exactly
    // as before multiple nudges existed; more get a line each, soonest first
    pub fn menu_lines(&self, nudges: &[(u32, &str)], now: Instant) -> Vec<String> {
        let showing = self.showing.is_some();
        if nudges.len() <= 1 {
            let label = if showing {
                "Reminder showing".to_string()
            } else if let Some(at) = self.soonest() {
                format!("Reminder in {}", hms(at.saturating_duration_since(now).as_secs()))
            } else {
                "Reminders off".to_string()
            };
            return vec![label];
        }
        if !self.on {
            let label = if showing { "Reminder showing" } else { "Reminders off" };
            return vec![label.to_string()];
        }
        // on screen sorts first (None), then by time
        let mut rows: Vec<(Option<Instant>, String)> = Vec::new();
        for &(id, message) in nudges {
            let name = short(message);
            if self.is_due_on_screen(id) {
                rows.push((None, format!("{name} · now")));
            } else if let Some(&at) = self.next.get(&id) {
                let left = hms(at.saturating_duration_since(now).as_secs());
                rows.push((Some(at), format!("{name} · {left}")));
            }
        }
        rows.sort_by_key(|row| row.0);
        if rows.is_empty() {
            let label = if showing { "Reminder showing" } else { "No reminders scheduled" };
            return vec![label.to_string()];
        }
        rows.into_iter().map(|row| row.1).collect()
    }
}

pub fn hms(secs: u64) -> String {
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

// a long message would stretch the whole menu
fn short(message: &str) -> String {
    const MAX: usize = 32;
    if message.chars().count() <= MAX {
        message.to_string()
    } else {
        message.chars().take(MAX - 1).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::{Schedule, Showing};
    use std::time::{Duration, Instant};

    const MIN: Duration = Duration::from_secs(60);

    fn on_with(now: Instant, timers: &[(u32, u64)]) -> Schedule {
        let mut schedule = Schedule::new();
        schedule.on = true;
        for &(id, secs) in timers {
            schedule.next.insert(id, now + Duration::from_secs(secs));
        }
        schedule
    }

    fn showing(look: u32, due: Vec<u32>, now: Instant) -> Showing {
        Showing {
            look,
            due,
            seq: 1,
            windows: vec!["overlay-1-0".into()],
            shown_at: now,
        }
    }

    #[test]
    fn nothing_is_due_before_the_first_timer() {
        let now = Instant::now();
        let schedule = on_with(now, &[(1, 30), (2, 90)]);
        assert!(schedule.due_now(now, MIN, false).is_empty());
    }

    #[test]
    fn a_due_nudge_brings_along_those_due_within_the_window() {
        let now = Instant::now();
        let schedule = on_with(now, &[(1, 45), (2, 0), (3, 61), (4, 600)]);
        // 2 is due now; 1 fires within a minute; 3 just misses it
        assert_eq!(schedule.due_now(now, MIN, false), vec![2, 1]);
    }

    #[test]
    fn a_break_now_takes_the_soonest_even_when_nothing_is_due() {
        let now = Instant::now();
        let schedule = on_with(now, &[(1, 900), (2, 300), (3, 330)]);
        assert_eq!(schedule.due_now(now, MIN, true), vec![2]);
        assert!(Schedule::new().due_now(now, MIN, true).is_empty());
    }

    #[test]
    fn opening_a_reminder_stops_its_nudges_waiting() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 0), (2, 20), (3, 900)]);
        let due = schedule.due_now(now, MIN, false);
        schedule.open(showing(due[0], due, now));
        assert_eq!(schedule.next.keys().copied().collect::<Vec<_>>(), vec![3]);
        assert_eq!(schedule.remaining(1, now), 0);
        assert_eq!(schedule.remaining(2, now), 0);
        assert_eq!(schedule.remaining(3, now), 900);
    }

    #[test]
    fn a_nudge_that_comes_due_during_a_reminder_joins_it() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 0), (2, 300)]);
        schedule.open(showing(1, vec![1], now));
        let later = now + Duration::from_secs(301);
        let joined = schedule.due_now(later, Duration::ZERO, false);
        schedule.join(&joined);
        assert_eq!(schedule.showing.as_ref().unwrap().due, vec![1, 2]);
        assert!(schedule.next.is_empty());
    }

    #[test]
    fn a_preview_answers_nothing_but_can_be_joined() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 600), (2, 5)]);
        schedule.open(showing(1, vec![], now));
        // the previewed nudge keeps counting down
        assert_eq!(schedule.remaining(1, now), 600);
        schedule.join(&schedule.due_now(now + Duration::from_secs(5), Duration::ZERO, false));
        assert_eq!(schedule.showing.as_ref().unwrap().due, vec![2]);
        assert_eq!(schedule.next.len(), 1);
    }

    #[test]
    fn dismissing_restarts_only_what_was_on_screen() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 0), (2, 900)]);
        schedule.open(showing(1, vec![1], now));
        let shown = schedule.close().unwrap();
        let later = now + Duration::from_secs(10);
        let timers: Vec<_> = shown.due.iter().map(|&id| (id, Duration::from_secs(1800))).collect();
        schedule.resume(&timers, later);
        assert_eq!(schedule.remaining(1, later), 1800);
        assert_eq!(schedule.remaining(2, later), 890);
    }

    #[test]
    fn nothing_starts_while_reminders_are_off() {
        let now = Instant::now();
        let mut schedule = Schedule::new();
        schedule.resume(&[(1, MIN)], now);
        schedule.pause(&[1], now + MIN);
        assert!(schedule.next.is_empty());
    }

    #[test]
    fn restarting_skips_nudges_still_on_screen() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 0), (2, 10)]);
        schedule.open(showing(1, vec![1], now));
        schedule.restart(&[(1, MIN), (2, MIN * 2)], now);
        assert_eq!(schedule.remaining(1, now), 0);
        assert_eq!(schedule.remaining(2, now), 120);
    }

    #[test]
    fn pausing_never_brings_a_nudge_forward() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 600), (2, 7200)]);
        schedule.pause(&[1, 2, 3], now + Duration::from_secs(3600));
        assert_eq!(schedule.remaining(1, now), 3600);
        assert_eq!(schedule.remaining(2, now), 7200);
        assert_eq!(schedule.remaining(3, now), 3600);
    }

    #[test]
    fn a_nudge_started_during_a_pause_waits_for_it() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 600)]);
        schedule.pause(&[1], now + Duration::from_secs(7200));
        // added or switched on in Settings mid-pause
        schedule.resume(&[(2, Duration::from_secs(1800))], now);
        assert_eq!(schedule.remaining(2, now), 7200);
        // after the pause a new timer runs its full interval
        let later = now + Duration::from_secs(7300);
        schedule.resume(&[(3, Duration::from_secs(1800))], later);
        assert_eq!(schedule.remaining(3, later), 1800);
        // turning reminders back on ends the pause
        schedule.restart(&[(1, Duration::from_secs(600))], now);
        assert_eq!(schedule.remaining(1, now), 600);
    }

    #[test]
    fn a_break_taken_during_a_pause_ends_it() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 600), (2, 900), (3, 9000)]);
        schedule.pause(&[1, 2, 3], now + Duration::from_secs(7200));
        // switched on in Settings mid-pause, so it waits for the pause too
        schedule.resume(&[(4, MIN * 30)], now);
        assert_eq!(schedule.remaining(4, now), 7200);
        let due = schedule.due_now(now, Duration::from_secs(60), true);
        schedule.open(showing(1, due, now));
        let timers = [(1, MIN * 30), (2, MIN * 20), (3, MIN * 60), (4, MIN * 30)];
        schedule.end_pause(&timers, now);
        // the pause held 2 and 4 back, so they start over; 3 was due after
        // the pause anyway and keeps its time
        assert_eq!(schedule.remaining(2, now), 1200);
        assert_eq!(schedule.remaining(4, now), 1800);
        assert_eq!(schedule.remaining(3, now), 9000);
        let shown = schedule.close().unwrap();
        let timers: Vec<_> = shown.due.iter().map(|&id| (id, Duration::from_secs(1800))).collect();
        schedule.resume(&timers, now);
        assert_eq!(schedule.remaining(1, now), 1800);
    }

    #[test]
    fn a_break_with_no_pause_moves_no_timer() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 600), (2, 900)]);
        schedule.end_pause(&[(1, MIN), (2, MIN)], now);
        assert_eq!(schedule.remaining(1, now), 600);
        assert_eq!(schedule.remaining(2, now), 900);
    }

    #[test]
    fn a_deleted_nudge_leaves_the_schedule_and_the_screen() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 0), (2, 0), (3, 60)]);
        schedule.open(showing(1, vec![1, 2], now));
        schedule.forget(2);
        schedule.forget(3);
        assert_eq!(schedule.showing.as_ref().unwrap().due, vec![1]);
        assert!(schedule.next.is_empty());
        assert_eq!(schedule.remaining(3, now), -1);
    }

    #[test]
    fn one_nudge_reads_as_it_always_has() {
        let now = Instant::now();
        let nudges = [(1, "Time to step away")];
        let mut schedule = on_with(now, &[(1, 1800)]);
        assert_eq!(schedule.menu_lines(&nudges, now), vec!["Reminder in 30:00"]);
        schedule.open(showing(1, vec![1], now));
        assert_eq!(schedule.menu_lines(&nudges, now), vec!["Reminder showing"]);
        schedule.close();
        schedule.on = false;
        schedule.next.clear();
        assert_eq!(schedule.menu_lines(&nudges, now), vec!["Reminders off"]);
    }

    #[test]
    fn several_nudges_get_a_line_each_soonest_first() {
        let now = Instant::now();
        let nudges = [(1, "Step away"), (2, "Drink some water"), (3, "Rest your eyes")];
        let mut schedule = on_with(now, &[(1, 4360), (2, 1085), (3, 402)]);
        assert_eq!(
            schedule.menu_lines(&nudges, now),
            vec!["Rest your eyes · 6:42", "Drink some water · 18:05", "Step away · 1:12:40"]
        );
        schedule.open(showing(1, vec![1], now));
        assert_eq!(schedule.menu_lines(&nudges, now)[0], "Step away · now");
    }

    #[test]
    fn several_nudges_collapse_to_one_line_when_off_or_idle() {
        let now = Instant::now();
        let nudges = [(1, "Step away"), (2, "Drink some water")];
        let mut schedule = on_with(now, &[]);
        assert_eq!(schedule.menu_lines(&nudges, now), vec!["No reminders scheduled"]);
        schedule.on = false;
        assert_eq!(schedule.menu_lines(&nudges, now), vec!["Reminders off"]);
    }

    #[test]
    fn long_messages_are_shortened_in_the_menu() {
        let now = Instant::now();
        let long = "Stand up, stretch and walk to the window for a bit";
        let nudges = [(1, long), (2, "Water")];
        let schedule = on_with(now, &[(1, 60)]);
        assert_eq!(schedule.menu_lines(&nudges, now), vec!["Stand up, stretch and walk to t… · 1:00"]);
    }

    #[test]
    fn a_pause_outlasts_a_wake_less_the_time_asleep() {
        let now = Instant::now();
        let hour = MIN * 60;
        let mut schedule = on_with(now, &[(1, 600)]);
        schedule.pause(&[1], now + hour * 2);
        // the clock this runs on stops while the Mac sleeps: an hour asleep
        // leaves an hour of the two-hour pause
        schedule.after_wake(&[(1, Duration::from_secs(600))], now, hour);
        assert_eq!(schedule.remaining(1, now), 3600);
    }

    #[test]
    fn a_pause_that_ran_out_during_sleep_is_over() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 600)]);
        schedule.pause(&[1], now + MIN * 30);
        schedule.after_wake(&[(1, Duration::from_secs(600))], now, MIN * 60);
        assert_eq!(schedule.remaining(1, now), 600);
        // nothing of the old pause holds a timer started now
        schedule.resume(&[(1, MIN)], now);
        assert_eq!(schedule.remaining(1, now), 60);
    }

    #[test]
    fn a_wake_without_a_restart_still_takes_the_sleep_off_a_pause() {
        let now = Instant::now();
        let hour = MIN * 60;
        let mut schedule = on_with(now, &[(1, 600), (2, 4 * 3600)]);
        schedule.pause(&[1, 2], now + hour * 2);
        let timers = [(1, Duration::from_secs(600)), (2, Duration::from_secs(4 * 3600))];
        schedule.shorten_pause(&timers, hour, now);
        assert_eq!(schedule.remaining(1, now), 3600, "held to the pause, which now ends in an hour");
        assert_eq!(schedule.remaining(2, now), 4 * 3600, "due after the pause anyway: untouched");
    }

    #[test]
    fn a_wake_with_no_pause_restarts_every_timer_in_full() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 30)]);
        schedule.after_wake(&[(1, Duration::from_secs(600))], now, MIN * 60);
        assert_eq!(schedule.remaining(1, now), 600);
    }

    #[test]
    fn a_pause_that_ran_out_asleep_does_not_greet_the_wake_with_a_reminder() {
        let now = Instant::now();
        let mut schedule = on_with(now, &[(1, 600)]);
        schedule.pause(&[1], now + MIN * 30);
        schedule.shorten_pause(&[(1, Duration::from_secs(600))], MIN * 60, now);
        assert_eq!(schedule.remaining(1, now), 600, "a full interval, not due at once");
    }
}
