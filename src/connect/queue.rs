//! The Connect queue: the context's tracks in play order, the current one,
//! and which of them the host has in its own queue (the window).

use rand::seq::SliceRandom;

/// Where an entry comes from, as Spotify's state names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Context,
    Queue,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Context => "context",
            Provider::Queue => "queue",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Base62 track id.
    pub id: String,
    /// The context's id of this entry (a track may come twice); generated
    /// when the context has none.
    pub uid: String,
    pub provider: Provider,
}

impl Entry {
    pub fn new(id: &str, uid: Option<&str>, provider: Provider) -> Self {
        let uid = uid.filter(|u| !u.is_empty()).map(str::to_string).unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()[..16].to_string());
        Entry { id: id.to_string(), uid, provider }
    }

    pub fn uri(&self) -> String {
        format!("spotify:track:{}", self.id)
    }
}

/// Which track to start from: by uid, then URI, then index.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkipTo {
    pub uid: Option<String>,
    pub uri: Option<String>,
    pub index: Option<usize>,
}

impl SkipTo {
    pub fn find(&self, entries: &[Entry]) -> usize {
        let by_uid = self.uid.as_deref().and_then(|u| entries.iter().position(|e| e.uid == u));
        let by_uri = self.uri.as_deref().and_then(|u| entries.iter().position(|e| e.uri() == u));
        by_uid.or(by_uri).or(self.index.filter(|&i| i < entries.len())).unwrap_or(0)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Queue {
    pub context_uri: String,
    pub entries: Vec<Entry>,
    pub current: usize,
    /// Context order before shuffling.
    unshuffled: Option<Vec<Entry>>,
    /// `[from, to)`: the entries in the host's queue, in this order.
    pub window: (usize, usize),
}

/// Tracks of the queue the state shows around the current one.
pub const MAX_PREV: usize = 10;
pub const MAX_NEXT: usize = 80;

impl Queue {
    pub fn new(context_uri: &str, entries: Vec<Entry>, current: usize) -> Self {
        let current = current.min(entries.len().saturating_sub(1));
        Queue { context_uri: context_uri.to_string(), entries, current, unshuffled: None, window: (0, 0) }
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.current)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn shuffled(&self) -> bool {
        self.unshuffled.is_some()
    }

    pub fn prev(&self) -> &[Entry] {
        &self.entries[self.current.saturating_sub(MAX_PREV)..self.current]
    }

    pub fn next(&self) -> &[Entry] {
        let from = (self.current + 1).min(self.entries.len());
        &self.entries[from..(from + MAX_NEXT).min(self.entries.len())]
    }

    /// The window to hand the host around the current entry: up to
    /// `before` entries before it and `after` after it.
    pub fn take_window(&mut self, before: usize, after: usize) -> (usize, usize) {
        let from = self.current.saturating_sub(before);
        let to = (self.current + 1 + after).min(self.entries.len());
        self.window = (from, to);
        self.window
    }

    /// Entries after the window, up to `n`, which then join it.
    pub fn extend_window(&mut self, n: usize) -> (usize, usize) {
        let (from, to) = self.window;
        let new_to = (to + n).min(self.entries.len());
        self.window = (from, new_to);
        (to, new_to)
    }

    /// The host now plays track `id`: the entry it is, nearest after the
    /// current one inside the window, else before. `false` if none is.
    pub fn follow(&mut self, id: &str) -> bool {
        let (from, to) = self.window;
        let to = to.min(self.entries.len());
        let after = (self.current.max(from)..to).find(|&i| self.entries[i].id == id);
        let before = (from..self.current.min(to)).rev().find(|&i| self.entries[i].id == id);
        match after.or(before) {
            Some(i) => {
                self.current = i;
                true
            }
            None => false,
        }
    }

    /// Insert right after the current entry (Spotify's "add to queue" as the
    /// host's "play next"), inside the window.
    pub fn add_next(&mut self, entry: Entry) {
        let at = (self.current + 1).min(self.entries.len());
        self.entries.insert(at, entry.clone());
        if let Some(u) = &mut self.unshuffled {
            u.push(entry);
        }
        if self.window.1 >= at && self.window.0 <= at {
            self.window.1 += 1;
        }
    }

    /// Replace everything after the current entry (Spotify's queue editor).
    pub fn set_next(&mut self, next: Vec<Entry>) {
        self.entries.truncate(self.current + 1);
        self.entries.extend(next);
        self.unshuffled = None;
    }

    /// Shuffle what follows the current entry, or put the context order back.
    pub fn shuffle(&mut self, on: bool) {
        if on == self.shuffled() || self.entries.is_empty() {
            return;
        }
        let current = self.entries[self.current].clone();
        if on {
            self.unshuffled = Some(self.entries.clone());
            let mut rest: Vec<Entry> = self.entries.iter().enumerate().filter(|(i, _)| *i != self.current).map(|(_, e)| e.clone()).collect();
            rest.shuffle(&mut rand::rng());
            self.entries = std::iter::once(current).chain(rest).collect();
            self.current = 0;
        } else {
            let original = self.unshuffled.take().unwrap_or_default();
            self.current = original.iter().position(|e| e.uid == current.uid).unwrap_or(0);
            self.entries = original;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(ids: &[&str], current: usize) -> Queue {
        Queue::new("spotify:album:x", ids.iter().map(|id| Entry::new(id, Some(&format!("u{id}")), Provider::Context)).collect(), current)
    }

    fn ids(es: &[Entry]) -> Vec<&str> {
        es.iter().map(|e| e.id.as_str()).collect()
    }

    #[test]
    fn start_track_by_uid_uri_or_index() {
        let es = q(&["a", "b", "c"], 0).entries;
        assert_eq!(SkipTo { uid: Some("uc".into()), ..Default::default() }.find(&es), 2);
        assert_eq!(SkipTo { uri: Some("spotify:track:b".into()), index: Some(2), ..Default::default() }.find(&es), 1);
        assert_eq!(SkipTo { index: Some(2), ..Default::default() }.find(&es), 2);
        assert_eq!(SkipTo { index: Some(9), ..Default::default() }.find(&es), 0);
        assert_eq!(SkipTo::default().find(&es), 0);
    }

    #[test]
    fn windows_and_following_the_host() {
        let mut queue = q(&["a", "b", "c", "a", "d", "e"], 2);
        assert_eq!(queue.take_window(1, 2), (1, 5));
        assert_eq!(ids(queue.prev()), ["a", "b"]);
        assert_eq!(ids(queue.next()), ["a", "d", "e"]);
        // `a` comes twice: the one after the current entry.
        assert!(queue.follow("a"));
        assert_eq!(queue.current, 3);
        assert!(queue.follow("b"), "back");
        assert_eq!(queue.current, 1);
        assert!(!queue.follow("e"), "outside the window");
        assert_eq!(queue.extend_window(5), (5, 6));
        assert!(queue.follow("e"));
        assert_eq!(queue.current, 5);
    }

    #[test]
    fn queue_edits() {
        let mut queue = q(&["a", "b", "c"], 0);
        queue.take_window(0, 10);
        queue.add_next(Entry::new("x", None, Provider::Queue));
        assert_eq!(ids(&queue.entries), ["a", "x", "b", "c"]);
        assert_eq!(queue.window, (0, 4), "in the host's queue too");
        assert_eq!(queue.entries[1].uid.len(), 16);
        queue.set_next(vec![Entry::new("c", Some("uc"), Provider::Context)]);
        assert_eq!(ids(&queue.entries), ["a", "c"]);

        let mut queue = q(&["a", "b", "c", "d", "e", "f"], 2);
        queue.shuffle(true);
        assert_eq!((queue.current, queue.entries[0].id.as_str()), (0, "c"), "the current track stays first");
        let mut sorted = ids(&queue.entries);
        sorted.sort();
        assert_eq!(sorted, ["a", "b", "c", "d", "e", "f"]);
        queue.shuffle(false);
        assert_eq!(ids(&queue.entries), ["a", "b", "c", "d", "e", "f"]);
        assert_eq!(queue.current, 2);
    }
}
