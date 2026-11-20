use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
struct LRUHandle<T>(usize, std::marker::PhantomData<T>);

struct LRUNode<T> {
    data: Option<T>,
    prev: Option<usize>,
    next: Option<usize>,
}

struct LRUList<T> {
    nodes: Vec<Option<LRUNode<T>>>,
    free: Vec<usize>,
    head: Option<usize>,
    tail: Option<usize>,
    count: usize,
}

impl<T> LRUList<T> {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            free: Vec::new(),
            head: None,
            tail: None,
            count: 0,
        }
    }

    /// Inserts new element at front (least recently used element)
    fn insert(&mut self, elem: T) -> LRUHandle<T> {
        let idx = if let Some(free_idx) = self.free.pop() {
            self.nodes[free_idx] = Some(LRUNode {
                data: Some(elem),
                prev: None,
                next: self.head,
            });
            free_idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(Some(LRUNode {
                data: Some(elem),
                prev: None,
                next: self.head,
            }));
            idx
        };

        if let Some(old_head) = self.head {
            if let Some(ref mut n) = self.nodes[old_head] {
                n.prev = Some(idx);
            }
        }
        self.head = Some(idx);
        if self.tail.is_none() {
            self.tail = Some(idx);
        }
        self.count += 1;
        LRUHandle(idx, std::marker::PhantomData)
    }

    fn remove(&mut self, node_handle: LRUHandle<T>) -> T {
        let idx = node_handle.0;
        let node = match self.nodes.get_mut(idx).and_then(|slot| slot.take()) {
            Some(n) => n,
            None => panic!("Attempted to remove non-existent node from LRUList"),
        };

        let prev = node.prev;
        let next = node.next;

        if let Some(p) = prev {
            if let Some(ref mut pn) = self.nodes[p] {
                pn.next = next;
            }
        } else {
            self.head = next;
        }

        if let Some(n) = next {
            if let Some(ref mut nn) = self.nodes[n] {
                nn.prev = prev;
            }
        } else {
            self.tail = prev;
        }

        self.free.push(idx);
        self.count -= 1;
        node.data.expect("node data missing")
    }

    fn remove_last(&mut self) -> Option<T> {
        let tail_idx = self.tail?;
        let node = self.nodes[tail_idx].take()?;
        let prev = node.prev;

        if let Some(p) = prev {
            if let Some(ref mut pn) = self.nodes[p] {
                pn.next = None;
            }
        } else {
            self.head = None;
        }
        self.tail = prev;

        self.free.push(tail_idx);
        self.count -= 1;
        node.data
    }

    /// Reinserts the referenced node at the front.
    fn reinsert_front(&mut self, node_handle: &LRUHandle<T>) {
        let idx = node_handle.0;
        if self.head == Some(idx) {
            return;
        }
        let (prev, next) = {
            let node = match self.nodes.get(idx) {
                Some(Some(n)) => n,
                _ => return,
            };
            (node.prev, node.next)
        };

        if let Some(p) = prev {
            if let Some(ref mut pn) = self.nodes[p] {
                pn.next = next;
            }
        } else {
            self.head = next;
        }

        if let Some(n) = next {
            if let Some(ref mut nn) = self.nodes[n] {
                nn.prev = prev;
            }
        } else {
            self.tail = prev;
        }

        if let Some(ref mut node) = self.nodes[idx] {
            node.prev = None;
            node.next = self.head;
        }
        if let Some(old_head) = self.head {
            if let Some(ref mut n) = self.nodes[old_head] {
                n.prev = Some(idx);
            }
        }
        self.head = Some(idx);
        if self.tail.is_none() {
            self.tail = Some(idx);
        }
    }

    fn count(&self) -> usize {
        self.count
    }

    fn _testing_head_ref(&self) -> Option<&T> {
        self.head
            .and_then(|idx| self.nodes[idx].as_ref())
            .and_then(|n| n.data.as_ref())
    }
}

pub type CacheKey = [u8; 16];
pub type CacheID = u64;
type CacheEntry<T> = (T, LRUHandle<CacheKey>);

/// Implementation of `ShardedLRUCache`.
/// Based on a HashMap; the elements are linked in order to support the LRU ordering.
pub struct Cache<T> {
    list: LRUList<CacheKey>,
    map: HashMap<CacheKey, CacheEntry<T>>,
    cap: usize,
    id: u64,
}

impl<T> Cache<T> {
    pub fn new(capacity: usize) -> Cache<T> {
        assert!(capacity > 0);
        Cache {
            list: LRUList::new(),
            map: HashMap::with_capacity(1024),
            cap: capacity,
            id: 0,
        }
    }

    /// Returns an ID that is unique for this cache and that can be used to partition the cache
    /// among several users.
    pub fn new_cache_id(&mut self) -> CacheID {
        self.id += 1;
        self.id
    }

    /// How many the cache currently contains
    pub fn count(&self) -> usize {
        self.list.count()
    }

    /// The capacity of this cache
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Insert a new element into the cache. The returned `CacheHandle` can be used for further
    /// operations on that element.
    /// If the capacity has been reached, the least recently used element is removed from the
    /// cache.
    pub fn insert(&mut self, key: &CacheKey, elem: T) {
        if let Some((_, old_handle)) = self.map.remove(key) {
            self.list.remove(old_handle);
        } else if self.list.count() >= self.cap {
            if let Some(removed_key) = self.list.remove_last() {
                self.map.remove(&removed_key);
            }
        }

        let lru_handle = self.list.insert(*key);
        self.map.insert(*key, (elem, lru_handle));
    }

    /// Retrieve an element from the cache.
    /// If the element has been preempted from the cache in the meantime, this returns None.
    pub fn get<'a>(&'a mut self, key: &CacheKey) -> Option<&'a T> {
        match self.map.get(key) {
            None => None,
            Some((elem, lru_handle)) => {
                self.list.reinsert_front(lru_handle);
                Some(elem)
            }
        }
    }

    /// Remove an element from the cache (for invalidation).
    pub fn remove(&mut self, key: &CacheKey) -> Option<T> {
        match self.map.remove(key) {
            None => None,
            Some((elem, lru_handle)) => {
                self.list.remove(lru_handle);
                Some(elem)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LRUList;
    use super::*;

    fn make_key(a: u8, b: u8, c: u8) -> CacheKey {
        [a, b, c, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    }

    #[test]
    fn test_blockcache_cache_add_rm() {
        let mut cache = Cache::new(128);

        let h_123 = make_key(1, 2, 3);
        let h_521 = make_key(1, 2, 4);
        let h_372 = make_key(3, 4, 5);
        let h_332 = make_key(6, 3, 1);
        let h_899 = make_key(8, 2, 1);

        cache.insert(&h_123, 123);
        cache.insert(&h_332, 332);
        cache.insert(&h_521, 521);
        cache.insert(&h_372, 372);
        cache.insert(&h_899, 899);

        assert_eq!(cache.count(), 5);

        assert_eq!(cache.get(&h_123), Some(&123));
        assert_eq!(cache.get(&h_372), Some(&372));

        assert_eq!(cache.remove(&h_521), Some(521));
        assert_eq!(cache.get(&h_521), None);
        assert_eq!(cache.remove(&h_521), None);

        assert_eq!(cache.count(), 4);
    }

    #[test]
    fn test_blockcache_cache_capacity() {
        let mut cache = Cache::new(3);

        let h_123 = make_key(1, 2, 3);
        let h_521 = make_key(1, 2, 4);
        let h_372 = make_key(3, 4, 5);
        let h_332 = make_key(6, 3, 1);
        let h_899 = make_key(8, 2, 1);

        cache.insert(&h_123, 123);
        cache.insert(&h_332, 332);
        cache.insert(&h_521, 521);
        cache.insert(&h_372, 372);
        cache.insert(&h_899, 899);

        assert_eq!(cache.count(), 3);

        assert_eq!(cache.get(&h_123), None);
        assert_eq!(cache.get(&h_332), None);
        assert_eq!(cache.get(&h_521), Some(&521));
        assert_eq!(cache.get(&h_372), Some(&372));
        assert_eq!(cache.get(&h_899), Some(&899));
    }

    #[test]
    fn test_blockcache_lru_remove() {
        let mut lru = LRUList::<usize>::new();

        let h_56 = lru.insert(56);
        lru.insert(22);
        lru.insert(223);
        let h_244 = lru.insert(244);
        lru.insert(1111);
        let h_12 = lru.insert(12);

        assert_eq!(lru.count(), 6);
        assert_eq!(244, lru.remove(h_244));
        assert_eq!(lru.count(), 5);
        assert_eq!(12, lru.remove(h_12));
        assert_eq!(lru.count(), 4);
        assert_eq!(56, lru.remove(h_56));
        assert_eq!(lru.count(), 3);
    }

    #[test]
    fn test_blockcache_lru_1() {
        let mut lru = LRUList::<usize>::new();

        lru.insert(56);
        lru.insert(22);
        lru.insert(244);
        lru.insert(12);

        assert_eq!(lru.count(), 4);

        assert_eq!(Some(56), lru.remove_last());
        assert_eq!(Some(22), lru.remove_last());
        assert_eq!(Some(244), lru.remove_last());

        assert_eq!(lru.count(), 1);

        assert_eq!(Some(12), lru.remove_last());

        assert_eq!(lru.count(), 0);

        assert_eq!(None, lru.remove_last());
    }

    #[test]
    fn test_blockcache_lru_reinsert() {
        let mut lru = LRUList::<usize>::new();

        let handle1 = lru.insert(56);
        let handle2 = lru.insert(22);
        let handle3 = lru.insert(244);

        assert_eq!(lru._testing_head_ref().copied().unwrap(), 244);

        lru.reinsert_front(&handle1);

        assert_eq!(lru._testing_head_ref().copied().unwrap(), 56);

        lru.reinsert_front(&handle3);

        assert_eq!(lru._testing_head_ref().copied().unwrap(), 244);

        lru.reinsert_front(&handle2);

        assert_eq!(lru._testing_head_ref().copied().unwrap(), 22);

        assert_eq!(lru.remove_last(), Some(56));
        assert_eq!(lru.remove_last(), Some(244));
        assert_eq!(lru.remove_last(), Some(22));
    }

    #[test]
    fn test_blockcache_lru_reinsert_2() {
        let mut lru = LRUList::<usize>::new();

        let handles = [
            lru.insert(0),
            lru.insert(1),
            lru.insert(2),
            lru.insert(3),
            lru.insert(4),
            lru.insert(5),
            lru.insert(6),
            lru.insert(7),
            lru.insert(8),
        ];

        (0..9).for_each(|i| {
            lru.reinsert_front(&handles[i]);
            assert_eq!(lru._testing_head_ref().copied(), Some(i));
        });
    }

    #[test]
    fn test_blockcache_lru_edge_cases() {
        let mut lru = LRUList::<usize>::new();

        let handle = lru.insert(3);

        lru.reinsert_front(&handle);
        assert_eq!(lru._testing_head_ref().copied(), Some(3));
        assert_eq!(lru.remove_last(), Some(3));
        assert_eq!(lru.remove_last(), None);
        assert_eq!(lru.remove_last(), None);
    }
}
