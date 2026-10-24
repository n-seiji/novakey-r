#![forbid(unsafe_code)]

// registry.rs - AI の結果を id でコントローラへ届けるための登録簿。
// 値は弱参照。強参照で持つと、IMK がコントローラを手放してもここに残って解放されない。
// 配送では、登録簿の借用を外してから呼び先を実行する (呼び先が登録簿に触れても二重借用にならない)。

use std::cell::RefCell;
use std::collections::HashMap;

pub struct Registry<W> {
    entries: RefCell<HashMap<u64, W>>,
}

impl<W> Registry<W> {
    pub fn new() -> Self {
        Self { entries: RefCell::new(HashMap::new()) }
    }

    /// 借用中 (run loop の再入) なら登録せず false を返す。
    pub fn insert(&self, id: u64, weak: W) -> bool {
        match self.entries.try_borrow_mut() {
            Ok(mut entries) => {
                entries.insert(id, weak);
                true
            }
            Err(_) => false,
        }
    }

    /// 借用中なら外さない。残った死んだエントリは、次の配送で掃除される。
    pub fn remove(&self, id: u64) {
        if let Ok(mut entries) = self.entries.try_borrow_mut() {
            entries.remove(&id);
        }
    }

    /// `id` の弱参照を `upgrade` で強参照にして、登録簿の借用を外してから `f` に渡す。
    /// upgrade できなかったエントリは外す。
    pub fn deliver<S>(&self, id: u64, upgrade: impl FnOnce(&W) -> Option<S>, f: impl FnOnce(S)) {
        let upgraded = {
            // 入れ子の run loop が登録処理の途中に再入しても、abort せず結果を捨てる。
            let Ok(entries) = self.entries.try_borrow() else {
                return;
            };
            let Some(weak) = entries.get(&id) else {
                return;
            };
            upgrade(weak)
        };
        match upgraded {
            Some(strong) => f(strong),
            None => self.remove(id),
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }
}

#[cfg(test)]
mod tests {
    use std::rc::{Rc, Weak};

    use super::Registry;

    fn deliver_to(registry: &Registry<Weak<String>>, id: u64, f: impl FnOnce(Rc<String>)) {
        registry.deliver(id, |w| w.upgrade(), f);
    }

    #[test]
    fn a_registered_target_receives_the_delivery() {
        let registry = Registry::new();
        let target = Rc::new("a".to_string());
        assert!(registry.insert(1, Rc::downgrade(&target)));
        let mut got = None;
        deliver_to(&registry, 1, |s| got = Some((*s).clone()));
        assert_eq!(got.as_deref(), Some("a"));
    }

    #[test]
    fn an_unknown_id_is_ignored() {
        let registry: Registry<Weak<String>> = Registry::new();
        let mut called = false;
        deliver_to(&registry, 9, |_| called = true);
        assert!(!called);
    }

    #[test]
    fn a_dead_entry_is_swept_on_delivery_and_nothing_is_called() {
        let registry = Registry::new();
        let target = Rc::new("a".to_string());
        registry.insert(1, Rc::downgrade(&target));
        drop(target);
        let mut called = false;
        deliver_to(&registry, 1, |_| called = true);
        assert!(!called);
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn removing_an_entry_stops_deliveries() {
        let registry = Registry::new();
        let target = Rc::new("a".to_string());
        registry.insert(1, Rc::downgrade(&target));
        registry.remove(1);
        let mut called = false;
        deliver_to(&registry, 1, |_| called = true);
        assert!(!called);
    }

    #[test]
    fn the_callee_may_touch_the_registry_during_delivery() {
        let registry = Registry::new();
        let a = Rc::new("a".to_string());
        let b = Rc::new("b".to_string());
        registry.insert(1, Rc::downgrade(&a));
        let mut inserted = false;
        let mut delivered_inner = false;
        deliver_to(&registry, 1, |_| {
            inserted = registry.insert(2, Rc::downgrade(&b));
            registry.remove(1);
            deliver_to(&registry, 2, |_| delivered_inner = true);
        });
        assert!(inserted);
        assert!(delivered_inner);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn insert_and_remove_during_upgrade_leave_the_registry_intact() {
        // upgrade の最中は借用中。再入した insert / remove / deliver は何もせず、panic もしない。
        let registry = Registry::new();
        let target = Rc::new("a".to_string());
        registry.insert(1, Rc::downgrade(&target));
        registry.deliver(
            1,
            |w| {
                assert!(!registry.insert(2, w.clone()));
                registry.remove(1);
                w.upgrade()
            },
            |_| {},
        );
        assert_eq!(registry.len(), 1);
    }
}
