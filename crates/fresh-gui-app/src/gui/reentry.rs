//! Nested GPUI entity updates.
//!
//! `Ctrl+W` handles `CloseTab` inside a `Workspace` update, and the dock then
//! calls `Panel::on_removed` / `Panel::set_active` before that update returns.
//! Those callbacks must not `Workspace::update` on the same stack. `cx.defer`
//! runs after the outermost update, which is the same pattern as publishing a
//! layout from `note_terminal_active`.

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, Context, Entity, TestAppContext, WeakEntity};

    struct Outer {
        inner: Option<Entity<Inner>>,
        updates: usize,
    }

    struct Inner {
        outer: WeakEntity<Outer>,
    }

    impl Inner {
        fn touch_now(&self, cx: &mut Context<Self>) {
            self.outer
                .update(cx, |outer, _| {
                    outer.updates += 1;
                })
                .expect("outer");
        }

        fn touch_later(&self, cx: &mut Context<Self>) {
            let outer = self.outer.clone();
            cx.defer(move |cx| {
                outer
                    .update(cx, |outer, _| {
                        outer.updates += 1;
                    })
                    .ok();
            });
        }
    }

    fn pair(cx: &mut TestAppContext) -> (Entity<Outer>, Entity<Inner>) {
        let outer = cx.new(|_| Outer {
            inner: None,
            updates: 0,
        });
        let inner = cx.new(|_| Inner {
            outer: outer.downgrade(),
        });
        cx.update(|cx| {
            outer.update(cx, |this, _| {
                this.inner = Some(inner.clone());
            });
        });
        (outer, inner)
    }

    /// The panic `Ctrl+W` hit: `Panel::on_removed` runs inside the `Workspace`
    /// update that closed the tab, and a nested `Workspace::update` aborts.
    /// Unwinding that panic drops the leased entity, so this case stays in its
    /// own test.
    #[gpui::test]
    #[should_panic(expected = "cannot update")]
    fn nested_workspace_update_panics(cx: &mut TestAppContext) {
        let (outer, inner) = pair(cx);
        cx.update(|cx| {
            outer.update(cx, |_, cx| {
                inner.update(cx, |inner, cx| inner.touch_now(cx));
            });
        });
    }

    /// `cx.defer` runs after the outermost update, which is how tab close and
    /// `set_active` write back to `Workspace`.
    #[gpui::test]
    fn deferred_workspace_update_runs_after_the_outer_update(cx: &mut TestAppContext) {
        let (outer, inner) = pair(cx);
        cx.update(|cx| {
            outer.update(cx, |this, cx| {
                inner.update(cx, |inner, cx| inner.touch_later(cx));
                assert_eq!(
                    this.updates, 0,
                    "defer must not run while Outer is still updating"
                );
            });
        });
        let updates = cx.read(|cx| outer.read(cx).updates);
        assert_eq!(updates, 1);
    }
}
