use std::any::Any;
use std::collections::{hash_map, BTreeMap, BTreeSet, HashMap};
use std::io::{Error as IoError, ErrorKind};
use std::sync::{Arc, Mutex};
use thiserror::Error;

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

impl Into<IoError> for RegistrationError {
    fn into(self) -> IoError {
        use RegistrationError::*;

        match self {
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

    /// Registers a root device within the inventory.
    ///
    /// Only one root device may exist in the inventory at once.
    pub fn register_root<T: Entity>(
        &self,
        ent: Arc<T>,
        name: String,
    ) -> Result<EntityID, RegistrationError> {
        let mut inv = self.inner.lock().unwrap();
        inv.register_root(ent, name)
    }

    /// Registers a new entity with the inventory.
    ///
    /// Returns an error if the object has already been registered.
    /// Returns an error if the parent is not registered.
    pub fn register<T: Entity>(
        &self,
        parent_id: EntityID,
        ent: Arc<T>,
        name: String,
    ) -> Result<EntityID, RegistrationError> {
        let mut inv = self.inner.lock().unwrap();
        inv.register(parent_id, ent, name)
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
    pub fn for_each_node<F>(&self, order: Order, func: F)
    where
        F: Fn(&Record),
    {
        let inv = self.inner.lock().unwrap();
        let mut iter = inv.iter(order);
        while let Some(record) = iter.next() {
            func(record);
        }
    }

    pub fn print(&self) {
        let inv = self.inner.lock().unwrap();
        inv.print()
    }
}

#[derive(Default)]
struct InventoryInner {
    root: Option<EntityID>,
    // Mapping of inventory-assigned ID to the entity's Record.
    entities: BTreeMap<EntityID, Record>,
    // Mapping of entity address to inventory-assigned ID.
    reverse: HashMap<usize, EntityID>,
    next_id: u64,
}

impl InventoryInner {
    /// Registers a root device within the inventory.
    ///
    /// Only one root device may exist in the inventory at once.
    pub fn register_root<T: Entity>(
        &mut self,
        ent: Arc<T>,
        name: String,
    ) -> Result<EntityID, RegistrationError> {
        let rec = Record::new(None, ent, name);

        if self.root.is_some() {
            return Err(RegistrationError::NotEmpty);
        }
        let id = self.next_id();
        self.insert(rec, id)?;
        self.root = Some(id);
        Ok(id)
    }

    /// Registers a new entity with the inventory.
    ///
    /// Returns an error if the object has already been registered.
    /// Returns an error if the parent is not registered.
    pub fn register<T: Entity>(
        &mut self,
        parent_id: EntityID,
        ent: Arc<T>,
        name: String,
    ) -> Result<EntityID, RegistrationError> {
        let rec = Record::new(Some(parent_id), ent, name);

        let id = self.next_id();

        self.insert(rec, id)?;
        self.insert_into_parent(parent_id, id)?;

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

    /// Removes an entity from the inventory.
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
                .ok_or_else(|| RegistrationError::MissingEntity(id))?;
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
        }

        // If this is the root, deregister it too.
        if let Some(root_id) = self.root.as_ref() {
            if *root_id == id {
                self.root = None;
            }
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

    fn insert_into_parent(
        &mut self,
        parent_id: EntityID,
        child_id: EntityID,
    ) -> Result<(), RegistrationError> {
        let parent = self
            .entities
            .get_mut(&parent_id)
            .ok_or_else(|| RegistrationError::MissingParent(parent_id))?;
        parent.children.insert(child_id);
        Ok(())
    }

    fn insert(
        &mut self,
        rec: Record,
        id: EntityID,
    ) -> Result<(), RegistrationError> {
        match self.reverse.entry(rec.ent.object_id()) {
            hash_map::Entry::Occupied(_) => {
                return Err(RegistrationError::AlreadyRegistered)
            }
            hash_map::Entry::Vacant(entry) => entry.insert(id),
        };
        self.entities.insert(id, rec);

        Ok(())
    }

    /// Iterates through the inventory according to the specified order.
    pub fn iter(&self, order: Order) -> EntityIterator<'_> {
        let stack = {
            if let Some(root_id) = self.root {
                vec![EntityIteratorNode::new(root_id, &self.entities)]
            } else {
                vec![]
            }
        };

        EntityIterator { inventory: self, stack, order }
    }

    pub fn print(&self) {
        for (id, rec) in self.entities.iter() {
            println!("{:x}: {} {:x?}", id.num, rec.name, rec.any);
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

struct EntityIteratorNode {
    id: EntityID,
    next_child: Option<EntityID>,
}

impl EntityIteratorNode {
    fn new(id: EntityID, entities: &BTreeMap<EntityID, Record>) -> Self {
        let record = entities.get(&id).unwrap();
        EntityIteratorNode { id, next_child: record.first_child() }
    }

    // Returns all children of the current node.
    // Does not recurse.
    fn get_next(&mut self, record: &Record) -> Option<EntityID> {
        if let Some(next) = self.next_child {
            self.next_child = record
                .children
                .range((
                    std::ops::Bound::Excluded(next),
                    std::ops::Bound::Unbounded,
                ))
                .next()
                .map(|x| *x);
            Some(next)
        } else {
            None
        }
    }
}

/// An iterator returned by [`InventoryInner::iter`].
pub struct EntityIterator<'a> {
    inventory: &'a InventoryInner,

    // Nodes, from root to the current node.
    stack: Vec<EntityIteratorNode>,

    order: Order,
}

impl<'a> Iterator for EntityIterator<'a> {
    type Item = &'a Record;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let node = self.stack.last_mut()?;
            let record = self.inventory.entities.get(&node.id).unwrap();

            match node.get_next(&record) {
                None => {
                    self.stack.pop();

                    // We found a leaf node *or* we finished viewing all
                    // children of a parent node.
                    //
                    // Post-Order: Always return.
                    // Pre-Order: Return only if this is leaf node.
                    if matches!(self.order, Order::Post)
                        || record.children.is_empty()
                    {
                        return Some(&record);
                    }
                }
                Some(next) => {
                    // More children exist, so they should be returned.
                    self.stack.push(EntityIteratorNode::new(
                        next,
                        &self.inventory.entities,
                    ));

                    // We found a node with children.
                    //
                    // Post-Order: Do nothing.
                    // Pre-Order: Return if we pushed the first child.
                    if matches!(self.order, Order::Pre)
                        && record.first_child().unwrap() == next
                    {
                        return Some(&record);
                    }
                }
            }
        }
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
    fn new<T: Entity>(
        parent: Option<EntityID>,
        ent: Arc<T>,
        name: String,
    ) -> Self {
        let any = Arc::clone(&ent) as Arc<dyn Any + Send + Sync>;
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

    fn first_child(&self) -> Option<EntityID> {
        self.children.iter().next().map(|x| *x)
    }
}

pub trait Block {}
pub trait Pci {}
pub trait Chipset {}
pub trait Uart {}

pub trait Entity: Send + Sync + 'static {
    fn as_block(&self) -> Option<&dyn Block> {
        None
    }
    fn as_pci(&self) -> Option<&dyn Pci> {
        None
    }
    fn as_chipset(&self) -> Option<&dyn Chipset> {
        None
    }
    fn as_uart(&self) -> Option<&dyn Uart> {
        None
    }
}

/// ID referencing an entity stored within the inventory.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct EntityID {
    num: u64,
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
        inv.register_root(entity, "root".to_string()).unwrap();
    }

    #[test]
    fn cannot_register_multiple_roots() {
        let entity = Arc::new(TestEntity {});
        let inv = Inventory::new();
        inv.register_root(entity.clone(), "root".to_string()).unwrap();
        assert_eq!(
            RegistrationError::NotEmpty,
            inv.register_root(entity, "child".to_string()).unwrap_err()
        );
    }

    #[test]
    fn register_root_and_child() {
        let root = Arc::new(TestEntity {});
        let child = Arc::new(TestEntity {});
        let inv = Inventory::new();

        let root_id =
            inv.register_root(root.clone(), "root".to_string()).unwrap();
        let child_id =
            inv.register(root_id, child.clone(), "child".to_string()).unwrap();

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

        let root_id = inv.register_root(root, "root".to_string()).unwrap();

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
        let root_id = inv.register_root(root, "root".to_string()).unwrap();
        let child_id =
            inv.register(root_id, child, "child".to_string()).unwrap();

        // ... But if the child disappears before we add the grandchild...
        inv.deregister(child_id).unwrap();

        // ... The registration of the grandchild entity will see a missing
        // parent error.
        assert_eq!(
            RegistrationError::MissingParent(child_id),
            inv.register(child_id, grandchild, "grandchild".to_string())
                .unwrap_err()
        );
    }

    #[test]
    fn double_registration_disallowed() {
        let root = Arc::new(TestEntity {});
        let child = Arc::new(TestEntity {});
        let inv = Inventory::new();

        let root_id =
            inv.register_root(root.clone(), "root".to_string()).unwrap();
        let _ =
            inv.register(root_id, child.clone(), "child".to_string()).unwrap();

        // Inserting the same object is disallowed.
        assert_eq!(
            RegistrationError::AlreadyRegistered,
            inv.register(root_id, child, "child".to_string()).unwrap_err()
        );

        // However, inserting new objects is still fine.
        let other_child = Arc::new(TestEntity {});
        inv.register(root_id, other_child, "other-child".to_string()).unwrap();
    }

    #[test]
    fn deregister_root_and_child() {
        let root = Arc::new(TestEntity {});
        let child = Arc::new(TestEntity {});
        let inv = Inventory::new();

        let root_id = inv.register_root(root, "root".to_string()).unwrap();
        inv.register(root_id, child, "child".to_string()).unwrap();
        inv.deregister(root_id).unwrap();

        // Peek into the internals of the inventory to check that it is empty.
        assert!(inv.is_empty());
    }

    #[test]
    fn double_deregister_throws_error() {
        let root = Arc::new(TestEntity {});
        let child = Arc::new(TestEntity {});
        let inv = Inventory::new();

        let root_id = inv.register_root(root, "root".to_string()).unwrap();
        let child_id =
            inv.register(root_id, child, "child".to_string()).unwrap();
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

    #[test]
    fn iterate_over_inventory_child_first() {
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
        let a_id = inv.register_root(a, "a".to_string()).unwrap();
        let b_id = inv.register(a_id, b, "b".to_string()).unwrap();
        let _ = inv.register(a_id, c, "c".to_string()).unwrap();
        let _ = inv.register(b_id, d, "d".to_string()).unwrap();
        let _ = inv.register(b_id, e, "e".to_string()).unwrap();

        let inv = inv.inner.lock().unwrap();
        let mut iter = inv.iter(Order::Post);
        assert_eq!(iter.next().unwrap().name(), "d");
        assert_eq!(iter.next().unwrap().name(), "e");
        assert_eq!(iter.next().unwrap().name(), "b");
        assert_eq!(iter.next().unwrap().name(), "c");
        assert_eq!(iter.next().unwrap().name(), "a");
        assert!(iter.next().is_none());
    }

    #[test]
    fn iterate_over_inventory_parent_first() {
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
        let a_id = inv.register_root(a, "a".to_string()).unwrap();
        let b_id = inv.register(a_id, b, "b".to_string()).unwrap();
        let _ = inv.register(a_id, c, "c".to_string()).unwrap();
        let _ = inv.register(b_id, d, "d".to_string()).unwrap();
        let _ = inv.register(b_id, e, "e".to_string()).unwrap();

        let inv = inv.inner.lock().unwrap();
        let mut iter = inv.iter(Order::Pre);
        assert_eq!(iter.next().unwrap().name(), "a");
        assert_eq!(iter.next().unwrap().name(), "b");
        assert_eq!(iter.next().unwrap().name(), "d");
        assert_eq!(iter.next().unwrap().name(), "e");
        assert_eq!(iter.next().unwrap().name(), "c");
        assert!(iter.next().is_none());
    }

    #[test]
    fn iterator_root_only_returns_root() {
        let root = Arc::new(TestEntity {});
        let inv = Inventory::new();
        let _ = inv.register_root(root, "root".to_string()).unwrap();

        let inv = inv.inner.lock().unwrap();
        let mut iter = inv.iter(Order::Pre);
        assert_eq!(iter.next().unwrap().name(), "root");
        assert!(iter.next().is_none());

        let mut iter = inv.iter(Order::Post);
        assert_eq!(iter.next().unwrap().name(), "root");
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
        let _ = inv.register_root(root.clone(), "root".to_string()).unwrap();

        inv.for_each_node(Order::Pre, |record| {
            assert_eq!(record.name(), "root");
            assert_eq!(record.concrete::<TestEntity>().unwrap(), root);
        });
    }
}
