use std::any::Any;
use std::collections::{btree_set, hash_map, BTreeMap, BTreeSet, HashMap};
use std::io::{Error as IoError, ErrorKind};
use std::sync::{Arc, Mutex};
use thiserror::Error;

use crate::dispatch::DispCtx;
use crate::instance::State;

/// Errors returned while registering or deregistering from [`Inventory`].
#[derive(Error, Debug, PartialEq)]
pub enum RegistrationError {
    #[error("Cannot re-register object")]
    AlreadyRegistered,

    #[error("Cannot find parent with ID: {0:?}")]
    MissingParent(EntityID),

    #[error("Cannot find entity with ID: {0:?}")]
    MissingEntity(EntityID),

    #[error("Cannot insert root into non-empty inventory")]
    NotEmpty,
}

impl From<RegistrationError> for IoError {
    fn from(e: RegistrationError) -> IoError {
        use RegistrationError::*;
        match e {
            AlreadyRegistered => {
                IoError::new(ErrorKind::AlreadyExists, "already registered")
            }
            MissingParent(_) => {
                IoError::new(ErrorKind::NotFound, "missing parent")
            }
            MissingEntity(_) => {
                IoError::new(ErrorKind::NotFound, "missing entity")
            }
            NotEmpty => {
                IoError::new(ErrorKind::AlreadyExists, "non-empty inventory")
            }
        }
    }
}

// Derives an address-based ID from an object which implements Entity.
trait ObjectID {
    fn object_id(&self) -> usize;
}

impl<T: Entity + ?Sized> ObjectID for Arc<T> {
    fn object_id(&self) -> usize {
        Arc::as_ptr(self) as *const () as usize
    }
}

/// A collection of virtual devices.
///
/// Inventory contains a collection of entities, which form a device tree.
pub struct Inventory {
    inner: Mutex<InventoryInner>,
}

impl Inventory {
    /// Initializes a new empty inventory.
    pub(crate) fn new() -> Self {
        Inventory { inner: Mutex::new(InventoryInner::default()) }
    }

    /// Registers a new entity with the inventory.
    ///
    /// Returns an error if the object has already been registered.
    /// Returns an error if the parent is not registered.
    pub fn register<T: Entity>(
        &self,
        ent: &Arc<T>,
        name: String,
        parent_id: Option<EntityID>,
    ) -> Result<EntityID, RegistrationError> {
        let mut inv = self.inner.lock().unwrap();
        let to_register = ent.child_register();
        let res = inv.register(ent, name, parent_id)?;

        if let Some(children) = to_register {
            for child in children {
                // Since the parent successfully registered, the children should
                // have no issues.
                inv.register_inner(
                    child.ent,
                    child.ent_any,
                    child.name,
                    Some(res),
                )
                .unwrap();
            }
        }
        Ok(res)
    }

    pub fn register_child(
        &self,
        reg: ChildRegister,
        parent_id: EntityID,
    ) -> Result<EntityID, RegistrationError> {
        let mut inv = self.inner.lock().unwrap();
        inv.register_inner(reg.ent, reg.ent_any, reg.name, Some(parent_id))
    }

    /// Access the concrete type of an entity by ID.
    ///
    /// Returns the entity if it exists and has the requested concrete type.
    pub fn get_concrete<T: Entity>(&self, id: EntityID) -> Option<Arc<T>> {
        let inv = self.inner.lock().unwrap();
        inv.get_concrete(id)
    }

    /// Removes an entity from the inventory.
    ///
    /// No-op if the entity does not exist.
    pub fn deregister(&self, id: EntityID) -> Result<(), RegistrationError> {
        let mut inv = self.inner.lock().unwrap();
        inv.deregister(id)
    }

    /// Returns true if the inventory is empty.
    pub fn is_empty(&self) -> bool {
        let inv = self.inner.lock().unwrap();
        inv.is_empty()
    }

    /// Looks up a record associated with `id`, and invokes `func` with the
    /// result.
    ///
    /// NOTE: `func` must not invoke any other methods on `&self`.
    pub fn for_record<F>(&self, id: EntityID, func: F)
    where
        F: FnOnce(Option<&Record>),
    {
        let inv = self.inner.lock().unwrap();
        func(inv.get(id))
    }

    /// Executes `func` for every record within the inventory.
    ///
    /// NOTE: `func` must not invoke any other methods on `&self`.
    pub fn for_each_node<F>(&self, order: Order, mut func: F)
    where
        F: FnMut(EntityID, &Record),
    {
        let inv = self.inner.lock().unwrap();
        for (eid, record) in inv.iter(order) {
            func(eid, record);
        }
    }

    pub fn print(&self) {
        let inv = self.inner.lock().unwrap();
        inv.print()
    }
}

#[derive(Default)]
struct InventoryInner {
    // Mapping of inventory-assigned ID to the entity's Record.
    entities: BTreeMap<EntityID, Record>,
    // Set of IDs for entities which have no parent
    roots: BTreeSet<EntityID>,
    // Mapping of entity address to inventory-assigned ID.
    reverse: HashMap<usize, EntityID>,
    next_id: u64,
}

impl InventoryInner {
    /// Registers a new entity with the inventory.
    ///
    /// Returns an error if the object has already been registered.
    /// Returns an error if the parent is not registered.
    pub fn register<T: Entity>(
        &mut self,
        ent: &Arc<T>,
        name: String,
        parent_id: Option<EntityID>,
    ) -> Result<EntityID, RegistrationError> {
        let any = Arc::clone(ent) as Arc<dyn Any + Send + Sync>;
        let dyn_ent = Arc::clone(ent) as Arc<dyn Entity>;
        self.register_inner(dyn_ent, any, name, parent_id)
    }

    fn register_inner(
        &mut self,
        ent: Arc<dyn Entity>,
        any: Arc<dyn Any + Send + Sync + 'static>,
        name: String,
        parent_id: Option<EntityID>,
    ) -> Result<EntityID, RegistrationError> {
        let id = self.next_id();
        let obj_id = ent.object_id();

        match self.reverse.entry(obj_id) {
            hash_map::Entry::Occupied(_) => {
                return Err(RegistrationError::AlreadyRegistered)
            }
            hash_map::Entry::Vacant(hme) => hme.insert(id),
        };

        if let Some(pid) = &parent_id {
            if let Some(parent) = self.entities.get_mut(pid) {
                parent.children.insert(id);
            } else {
                // undo entry in reverse table before bailing
                self.reverse.remove(&obj_id).unwrap();
                return Err(RegistrationError::MissingParent(*pid));
            }
        } else {
            self.roots.insert(id);
        }

        let rec = Record::new(ent, any, name, parent_id);
        self.entities.insert(id, rec);

        Ok(id)
    }

    /// Access the concrete type of an entity by ID.
    ///
    /// Returns the entity if it exists and has the requested concrete type.
    pub fn get_concrete<T: Entity>(&self, id: EntityID) -> Option<Arc<T>> {
        self.entities.get(&id)?.concrete()
    }

    /// Access an entity's record by ID.
    pub fn get(&self, id: EntityID) -> Option<&Record> {
        let record = self.entities.get(&id)?;
        Some(record)
    }

    /// Removes an entity (and all its children) from the inventory.
    ///
    /// No-op if the entity does not exist.
    pub fn deregister(
        &mut self,
        id: EntityID,
    ) -> Result<(), RegistrationError> {
        // Remove all children recursively.
        let children = {
            let record = self
                .entities
                .get_mut(&id)
                .ok_or(RegistrationError::MissingEntity(id))?;
            std::mem::take(&mut record.children)
        };
        for child in &children {
            self.deregister(*child)?;
        }

        // Remove the current entity.
        let record = self.entities.remove(&id).unwrap();
        let entptr = record.ent.object_id();
        self.reverse.remove(&entptr);

        // If this entity exists in the parent, remove it.
        //
        // Note that the parent may not exist, or it may have already been
        // removed as a part of the recursive walk.
        if let Some(parent) = record.parent {
            let parent = self.entities.get_mut(&parent).unwrap();
            parent.children.remove(&id);
        } else {
            self.roots.remove(&id);
        }

        Ok(())
    }

    /// Returns true if the inventory is empty.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    fn next_id(&mut self) -> EntityID {
        self.next_id += 1;
        EntityID { num: self.next_id }
    }

    /// Iterates through the inventory according to the specified order.
    pub fn iter(&self, order: Order) -> Iter<'_> {
        Iter::new(self, order)
    }

    pub fn print(&self) {
        let mut stack: Vec<EntityID> = Vec::new();
        for (id, rec) in self.iter(Order::Pre) {
            let rec_parent = rec.parent();
            while let Some(eid) = stack.pop() {
                if matches!(rec_parent, Some(id) if id == eid) {
                    stack.push(eid);
                    break;
                }
            }
            stack.push(id);

            let depth = stack.len() - 1;
            println!(
                "{}- {}: {}",
                "  ".repeat(depth),
                u64::from(id),
                rec.name()
            );
        }
    }
}

/// Describes the iteration order through the inventory.
pub enum Order {
    /// Children before parent nodes
    Post,
    /// Parent before any child nodes
    Pre,
}

struct IterNode<'a> {
    post_visit: Option<(EntityID, &'a Record)>,
    items: btree_set::Iter<'a, EntityID>,
}

pub struct Iter<'a> {
    inv: &'a InventoryInner,
    stack: Vec<IterNode<'a>>,
    order: Order,
}

impl<'a> Iter<'a> {
    fn new(inv: &'a InventoryInner, order: Order) -> Self {
        Self {
            stack: vec![IterNode { post_visit: None, items: inv.roots.iter() }],
            inv,
            order,
        }
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = (EntityID, &'a Record);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(mut node) = self.stack.pop() {
            if let Some(eid) = node.items.next() {
                self.stack.push(node);

                let rec = self.inv.entities.get(eid).unwrap();
                if !rec.children.is_empty() {
                    if matches!(self.order, Order::Pre) {
                        // Visit the children of this node after it is emitted.
                        self.stack.push(IterNode {
                            post_visit: None,
                            items: rec.children.iter(),
                        });
                        return Some((*eid, rec));
                    } else {
                        // Proceed deeper into the tree, taking note of our
                        // parent so it can be processed on the way back out
                        self.stack.push(IterNode {
                            post_visit: Some((*eid, rec)),
                            items: rec.children.iter(),
                        });
                    }
                } else {
                    // Emit leaf entry
                    return Some((*eid, rec));
                }
            } else {
                // Done with all children for a node, perform the post-ordered
                // visit, if necessary.
                if let Some(post) = node.post_visit.take() {
                    debug_assert!(matches!(self.order, Order::Post));
                    return Some(post);
                }
            }
        }
        None
    }
}

/// A device node within the inventory.
pub struct Record {
    any: Arc<dyn Any + Send + Sync + 'static>,
    ent: Arc<dyn Entity>,
    parent: Option<EntityID>,
    children: BTreeSet<EntityID>,
    name: String,
}

impl Record {
    fn new(
        ent: Arc<dyn Entity>,
        any: Arc<dyn Any + Send + Sync + 'static>,
        name: String,
        parent: Option<EntityID>,
    ) -> Self {
        Record { any, ent, parent, children: BTreeSet::new(), name }
    }

    /// Returns the concrete type of a record, or None if the wrong type is
    /// specified.
    pub fn concrete<T: Entity>(&self) -> Option<Arc<T>> {
        self.any.clone().downcast::<T>().ok()
    }

    /// Returns the entity-based type of the record.
    pub fn entity(&self) -> &Arc<dyn Entity> {
        &self.ent
    }

    /// Returns the name of the record used during registration.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn parent(&self) -> Option<EntityID> {
        self.parent
    }
}

pub trait Entity: Send + Sync + 'static {
    #[allow(unused_variables)]
    fn state_transition(
        &self,
        next: State,
        target: Option<State>,
        ctx: &DispCtx,
    ) {
    }
    #[allow(unused_variables)]
    fn child_register(&self) -> Option<Vec<ChildRegister>> {
        None
    }
}

pub struct ChildRegister {
    ent: Arc<dyn Entity>,
    ent_any: Arc<dyn Any + Send + Sync + 'static>,
    name: String,
}
impl ChildRegister {
    pub fn new<T: Entity>(ent: &Arc<T>, name: String) -> Self {
        Self {
            ent: Arc::clone(ent) as Arc<dyn Entity>,
            ent_any: Arc::clone(ent) as Arc<dyn Any + Send + Sync + 'static>,
            name,
        }
    }
}

/// ID referencing an entity stored within the inventory.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct EntityID {
    num: u64,
}

impl From<EntityID> for u64 {
    fn from(id: EntityID) -> Self {
        id.num
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct TestEntity {}
    impl Entity for TestEntity {}

    #[test]
    fn register_root() {
        let entity = Arc::new(TestEntity {});
        let inv = Inventory::new();
        inv.register(&entity, "root".to_string(), None).unwrap();
    }

    #[test]
    fn register_multiple_roots() {
        let inv = Inventory::new();

        let ent1 = Arc::new(TestEntity {});
        let ent2 = Arc::new(TestEntity {});
        let ent3 = Arc::new(TestEntity {});

        inv.register(&ent1, "root1".to_string(), None).unwrap();
        inv.register(&ent2, "root2".to_string(), None).unwrap();
        inv.register(&ent3, "root3".to_string(), None).unwrap();
    }

    #[test]
    fn register_root_and_child() {
        let root = Arc::new(TestEntity {});
        let child = Arc::new(TestEntity {});
        let inv = Inventory::new();

        let root_id = inv.register(&root, "root".to_string(), None).unwrap();
        let child_id =
            inv.register(&child, "child".to_string(), Some(root_id)).unwrap();

        // The root is accessible as an entity, or as a concrete object.
        inv.for_record(root_id, |maybe_record| assert!(maybe_record.is_some()));
        let concrete_root = inv.get_concrete::<TestEntity>(root_id).unwrap();
        assert!(Arc::ptr_eq(&root, &concrete_root));

        // The child is accessible as an entity, or as a concrete object.
        inv.for_record(
            child_id,
            |maybe_record| assert!(maybe_record.is_some()),
        );
        let concrete_child = inv.get_concrete::<TestEntity>(child_id).unwrap();
        assert!(Arc::ptr_eq(&child, &concrete_child));
    }

    #[test]
    fn get_concrete_wrong_type_returns_none() {
        let root = Arc::new(TestEntity {});
        let inv = Inventory::new();

        let root_id = inv.register(&root, "root".to_string(), None).unwrap();

        struct OtherEntity {}
        impl Entity for OtherEntity {}
        assert!(inv.get_concrete::<OtherEntity>(root_id).is_none());
    }

    #[test]
    fn register_with_missing_parent() {
        let root = Arc::new(TestEntity {});
        let child = Arc::new(TestEntity {});
        let grandchild = Arc::new(TestEntity {});
        let inv = Inventory::new();

        // Register the root and child...
        let root_id = inv.register(&root, "root".to_string(), None).unwrap();
        let child_id =
            inv.register(&child, "child".to_string(), Some(root_id)).unwrap();

        // ... But if the child disappears before we add the grandchild...
        inv.deregister(child_id).unwrap();

        // ... The registration of the grandchild entity will see a missing
        // parent error.
        assert_eq!(
            RegistrationError::MissingParent(child_id),
            inv.register(&grandchild, "grandchild".to_string(), Some(child_id))
                .unwrap_err()
        );
    }

    #[test]
    fn double_registration_disallowed() {
        let root = Arc::new(TestEntity {});
        let child = Arc::new(TestEntity {});
        let inv = Inventory::new();

        let root_id = inv.register(&root, "root".to_string(), None).unwrap();
        let _ =
            inv.register(&child, "child".to_string(), Some(root_id)).unwrap();

        // Inserting the same object is disallowed.
        assert_eq!(
            RegistrationError::AlreadyRegistered,
            inv.register(&child, "child".to_string(), Some(root_id))
                .unwrap_err()
        );

        // However, inserting new objects is still fine.
        let other_child = Arc::new(TestEntity {});
        inv.register(&other_child, "other-child".to_string(), Some(root_id))
            .unwrap();
    }

    #[test]
    fn deregister_root_and_child() {
        let root = Arc::new(TestEntity {});
        let child = Arc::new(TestEntity {});
        let inv = Inventory::new();

        let root_id = inv.register(&root, "root".to_string(), None).unwrap();
        inv.register(&child, "child".to_string(), Some(root_id)).unwrap();
        inv.deregister(root_id).unwrap();

        // Peek into the internals of the inventory to check that it is empty.
        assert!(inv.is_empty());
    }

    #[test]
    fn double_deregister_throws_error() {
        let root = Arc::new(TestEntity {});
        let child = Arc::new(TestEntity {});
        let inv = Inventory::new();

        let root_id = inv.register(&root, "root".to_string(), None).unwrap();
        let child_id =
            inv.register(&child, "child".to_string(), Some(root_id)).unwrap();
        inv.deregister(root_id).unwrap();

        // Neither the root nor child device may be deregistered multiple times.
        assert_eq!(
            RegistrationError::MissingEntity(root_id),
            inv.deregister(root_id).unwrap_err()
        );
        assert_eq!(
            RegistrationError::MissingEntity(child_id),
            inv.deregister(child_id).unwrap_err()
        );
    }

    fn build_iter_tree() -> Inventory {
        let a = Arc::new(TestEntity {});
        let b = Arc::new(TestEntity {});
        let c = Arc::new(TestEntity {});
        let d = Arc::new(TestEntity {});
        let e = Arc::new(TestEntity {});
        let inv = Inventory::new();

        // Tree structure:
        //
        //     A
        //    / \
        //   B   C
        //  / \
        // D   E
        let a_id = inv.register(&a, "a".to_string(), None).unwrap();
        let b_id = inv.register(&b, "b".to_string(), Some(a_id)).unwrap();
        let _ = inv.register(&c, "c".to_string(), Some(a_id)).unwrap();
        let _ = inv.register(&d, "d".to_string(), Some(b_id)).unwrap();
        let _ = inv.register(&e, "e".to_string(), Some(b_id)).unwrap();
        inv
    }

    #[test]
    fn iterate_over_inventory_child_first() {
        let test_inv = build_iter_tree();
        let inv = test_inv.inner.lock().unwrap();
        let mut iter = inv.iter(Order::Post);
        assert_eq!(iter.next().unwrap().1.name(), "d");
        assert_eq!(iter.next().unwrap().1.name(), "e");
        assert_eq!(iter.next().unwrap().1.name(), "b");
        assert_eq!(iter.next().unwrap().1.name(), "c");
        assert_eq!(iter.next().unwrap().1.name(), "a");
        assert!(iter.next().is_none());
    }

    #[test]
    fn iterate_over_inventory_parent_first() {
        let test_inv = build_iter_tree();
        let inv = test_inv.inner.lock().unwrap();
        let mut iter = inv.iter(Order::Pre);
        assert_eq!(iter.next().unwrap().1.name(), "a");
        assert_eq!(iter.next().unwrap().1.name(), "b");
        assert_eq!(iter.next().unwrap().1.name(), "d");
        assert_eq!(iter.next().unwrap().1.name(), "e");
        assert_eq!(iter.next().unwrap().1.name(), "c");
        assert!(iter.next().is_none());
    }

    #[test]
    fn iterator_root_only_returns_root() {
        let root = Arc::new(TestEntity {});
        let inv = Inventory::new();
        let _ = inv.register(&root, "root".to_string(), None).unwrap();

        let inv = inv.inner.lock().unwrap();
        let mut iter = inv.iter(Order::Pre);
        assert_eq!(iter.next().unwrap().1.name(), "root");
        assert!(iter.next().is_none());

        let mut iter = inv.iter(Order::Post);
        assert_eq!(iter.next().unwrap().1.name(), "root");
        assert!(iter.next().is_none());
    }

    #[test]
    fn iterator_empty_returns_none() {
        let inv = Inventory::new();
        assert!(inv.inner.lock().unwrap().iter(Order::Pre).next().is_none());
        assert!(inv.inner.lock().unwrap().iter(Order::Post).next().is_none());
    }

    #[test]
    fn for_each_node() {
        let inv = Inventory::new();
        let root = Arc::new(TestEntity {});
        let _ = inv.register(&root, "root".to_string(), None).unwrap();

        inv.for_each_node(Order::Pre, |_eid, record| {
            assert_eq!(record.name(), "root");
            assert_eq!(record.concrete::<TestEntity>().unwrap(), root);
        });
    }
}
