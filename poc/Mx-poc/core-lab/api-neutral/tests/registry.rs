use api_neutral::{BindingRegistry, BoxedMessage, BoxedResult, ErasedActor};
struct Echo;
impl ErasedActor for Echo {
    fn receive(&mut self, m: BoxedMessage) -> BoxedResult {
        Ok(m)
    }
}
#[test]
fn bindings_are_unique_sorted_and_owned() {
    let registry = BindingRegistry::new();
    let z = registry.register("z", || Ok(Box::new(Echo))).unwrap();
    let a = registry.register("a", || Ok(Box::new(Echo))).unwrap();
    assert_eq!(registry.list(), vec!["a", "z"]);
    assert!(registry.register("a", || Ok(Box::new(Echo))).is_err());
    drop(a);
    assert_eq!(registry.list(), vec!["z"]);
    drop(z);
    assert!(registry.list().is_empty());
}
#[test]
fn rejected_and_removed_factory_captures_can_reenter_registry() {
    struct Reenter(BindingRegistry);
    impl Drop for Reenter {
        fn drop(&mut self) {
            let _ = self.0.list();
        }
    }
    let registry = BindingRegistry::new();
    let original = registry.register("a", || Ok(Box::new(Echo))).unwrap();
    let rejected = Reenter(registry.clone());
    assert!(registry
        .register("a", move || {
            let _ = &rejected;
            Ok(Box::new(Echo))
        })
        .is_err());
    drop(original);
    let capture = Reenter(registry.clone());
    let token = registry
        .register("a", move || {
            let _ = &capture;
            Ok(Box::new(Echo))
        })
        .unwrap();
    drop(token);
    assert!(registry.list().is_empty());
}
