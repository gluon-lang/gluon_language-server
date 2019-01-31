//! Collection types which allows insertion of new values while shared references to its contents
//! are alive. This is done by storing each value in a stable memory location and preventing an
//! earlier inserted value to be overwritten.
use std::{
    borrow::Borrow,
    cell::RefCell,
    collections::hash_map::Entry,
    fmt,
    hash::Hash,
    iter::IntoIterator,
    mem,
    ops::{Index, IndexMut},
};

use crate::fnv::FnvMap;
use vec_map::VecMap;

// NOTE: transmute is used to circumvent the borrow checker in this module
// This is safe since the containers hold boxed values meaning allocating larger
// storage does not invalidate the references that are handed out and because values
// can only be inserted, never modified (this could be done with a Rc pointer as well but
// is not done for efficiency since it is not needed)

unsafe fn forget_lifetime<'a, 'b, T: ?Sized>(x: &'a T) -> &'b T {
    ::std::mem::transmute(x)
}

// A mapping between K and V where once a value has been inserted it cannot be changed
// Through this and the fact the all values are stored as pointers it is possible to safely
// insert new values without invalidating pointers retrieved from it
pub struct FixedMap<K, V> {
    map: RefCell<FnvMap<K, (u32, u32)>>,
    values: Buffer<V>,
}

impl<K: Eq + Hash, V> Default for FixedMap<K, V> {
    fn default() -> FixedMap<K, V> {
        FixedMap::new()
    }
}

impl<K: Eq + Hash + fmt::Debug, V: fmt::Debug> fmt::Debug for FixedMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.map.borrow().fmt(f)
    }
}

impl<K: Eq + Hash, V> FixedMap<K, V> {
    pub fn new() -> FixedMap<K, V> {
        FixedMap {
            map: RefCell::new(FnvMap::default()),
            values: Default::default(),
        }
    }

    pub fn clear(&mut self) {
        self.map.borrow_mut().clear();
    }

    pub fn len(&self) -> usize {
        self.map.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<K: Eq + Hash, V> FixedMap<K, V> {
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        match self.map.get_mut().entry(key) {
            Entry::Occupied(entry) => Some(mem::replace(&mut self.values[*entry.get()], value)),
            Entry::Vacant(entry) => {
                entry.insert(self.values.push(value));
                None
            }
        }
    }

    pub fn try_insert(&self, key: K, value: V) -> Result<(), (K, V)> {
        if self.get(&key).is_some() {
            Err((key, value))
        } else {
            let index_value = self.values.push(value);
            self.map.borrow_mut().insert(key, index_value);
            Ok(())
        }
    }

    pub fn get<Q>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Eq + Hash,
    {
        self.map.borrow().get(k).map(|key| &self.values[*key])
    }

    pub fn get_mut<Q>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash,
    {
        let values = &mut self.values;
        self.map.get_mut().get(k).map(move |&key| &mut values[key])
    }
}

impl<'a, Q, K, V> Index<&'a Q> for FixedMap<K, V>
where
    K: Eq + Hash + Borrow<Q>,
    Q: ?Sized + Eq + Hash,
{
    type Output = V;
    fn index(&self, index: &'a Q) -> &Self::Output {
        self.get(index).expect("Index out of bounds")
    }
}

// A mapping between K and V where once a value has been inserted it cannot be changed
// Through this and the fact the all values are stored as pointers it is possible to safely
// insert new values without invalidating pointers retrieved from it
pub struct FixedVecMap<V> {
    // Use u16 to leave space for the `Option` tag in `VecMap`
    map: RefCell<VecMap<(u16, u32)>>,
    values: Buffer<V>,
}

impl<V> Default for FixedVecMap<V> {
    fn default() -> FixedVecMap<V> {
        FixedVecMap::new()
    }
}

impl<V: fmt::Debug> fmt::Debug for FixedVecMap<V> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.map.borrow().fmt(f)
    }
}

impl<V> FixedVecMap<V> {
    pub fn new() -> FixedVecMap<V> {
        FixedVecMap {
            map: Default::default(),
            values: Default::default(),
        }
    }

    pub fn clear(&mut self) {
        self.map.borrow_mut().clear();
    }

    pub fn len(&self) -> usize {
        self.map.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<V> FixedVecMap<V> {
    pub fn insert(&mut self, key: usize, value: V) -> Option<V> {
        use vec_map::Entry;
        match self.map.get_mut().entry(key) {
            Entry::Occupied(entry) => Some(mem::replace(&mut self.values[*entry.get()], value)),
            Entry::Vacant(entry) => {
                let (i, j) = self.values.push(value);
                entry.insert((i as u16, j));
                None
            }
        }
    }

    pub fn try_insert(&self, key: usize, value: V) -> Result<(), (usize, V)> {
        if self.get(key).is_some() {
            Err((key, value))
        } else {
            let (i, j) = self.values.push(value);
            self.map.borrow_mut().insert(key, (i as u16, j));
            Ok(())
        }
    }

    pub fn get(&self, k: usize) -> Option<&V> {
        self.map.borrow().get(k).map(|key| &self.values[*key])
    }

    pub fn get_mut(&mut self, k: usize) -> Option<&mut V> {
        let values = &mut self.values;
        self.map.get_mut().get(k).map(move |&key| &mut values[key])
    }

    pub fn remove(&mut self, k: usize) -> Option<V> {
        let values = self.values.values.get_mut();
        self.map.get_mut().remove(k).and_then(|(i, j)| {
            if values.len() == i as usize + 1 && values[i as usize].len() == j as usize + 1 {
                let x = values[i as usize].pop();
                if values[i as usize].is_empty() {
                    values.pop();
                }
                x
            } else {
                unimplemented!()
            }
        })
    }

    pub fn truncate(&mut self, index: usize) {
        self.map.get_mut().retain(|i, _| i < index);
        self.values.truncate(index);
    }

    pub fn drain<'a>(&'a mut self) -> impl Iterator<Item = V> + 'a {
        self.map.get_mut().clear();
        self.values.drain()
    }
}

impl<V> Index<usize> for FixedVecMap<V> {
    type Output = V;
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("Index out of bounds")
    }
}

#[derive(Debug)]
pub struct FixedVec<T> {
    vec: RefCell<Vec<(u32, u32)>>,
    values: Buffer<T>,
}

impl<T> Default for FixedVec<T> {
    fn default() -> Self {
        FixedVec::new()
    }
}

impl<T> FixedVec<T> {
    pub fn new() -> FixedVec<T> {
        FixedVec {
            vec: RefCell::new(Vec::new()),
            values: Default::default(),
        }
    }

    pub fn clear(&mut self) {
        self.vec.borrow_mut().clear();
    }

    pub fn push(&self, value: T) {
        let key = self.values.push(value);
        self.vec.borrow_mut().push(key);
    }

    pub fn extend<I: IntoIterator<Item = T>>(&self, iter: I) {
        for item in iter {
            self.push(item);
        }
    }

    pub fn len(&self) -> usize {
        self.vec.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        (0..).scan((), move |_, i| self.get(i))
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        if index < self.len() {
            Some(&self[index])
        } else {
            None
        }
    }

    pub fn truncate(&mut self, index: usize) {
        self.vec.get_mut().truncate(index);
        self.values.truncate(index)
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.vec.get_mut().pop().is_some() {
            self.values.pop()
        } else {
            None
        }
    }
}

impl<T> Index<usize> for FixedVec<T> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        let vec = self.vec.borrow();
        &self.values[vec[index]]
    }
}

impl<T> IndexMut<usize> for FixedVec<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        let vec = self.vec.borrow();
        &mut self.values[vec[index]]
    }
}

#[derive(Debug)]
struct Buffer<T> {
    values: RefCell<Vec<Vec<T>>>,
}

impl<T> Default for Buffer<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Buffer<T> {
    pub fn new() -> Self {
        Self {
            values: Default::default(),
        }
    }

    fn total_len(&self) -> usize {
        self.values
            .borrow()
            .iter()
            .map(|vec| vec.len())
            .sum::<usize>()
    }

    fn push(&self, value: T) -> (u32, u32) {
        let mut values = self.values.borrow_mut();
        let cap = match values.last() {
            Some(vec) => {
                if vec.len() == vec.capacity() {
                    Some(vec.capacity() * 2)
                } else {
                    None
                }
            }
            None => Some(4),
        };
        if let Some(cap) = cap {
            values.push(Vec::with_capacity(cap));
        }

        let i = values.len() as u32 - 1;
        let inner = values.last_mut().unwrap();
        let j = inner.len() as u32;
        inner.push(value);
        (i, j)
    }

    fn truncate(&mut self, index: usize) {
        let mut left = self.total_len() - index;
        let values = self.values.get_mut();
        while left != 0 {
            let inner = values.last_mut().expect("Not values left");
            if inner.len() <= left {
                left -= inner.len();
                values.pop();
            } else {
                let i = inner.len() - left;
                inner.truncate(i);
                left = 0;
            }
        }
    }

    fn drain<'a>(&'a mut self) -> impl Iterator<Item = T> + 'a {
        self.values.get_mut().drain(..).flat_map(|vec| vec)
    }

    fn pop(&mut self) -> Option<T> {
        let values = self.values.get_mut();
        let out = values.last_mut().and_then(|vec| vec.pop());
        if out.is_some() {
            out
        } else {
            values.pop();
            values.last_mut().and_then(|vec| vec.pop())
        }
    }
}

impl<T> Index<(u32, u32)> for Buffer<T> {
    type Output = T;
    fn index(&self, (i, j): (u32, u32)) -> &T {
        unsafe {
            forget_lifetime(
                &self
                    .values
                    .borrow()
                    .get(i as usize)
                    .and_then(|v| v.get(j as usize))
                    .unwrap_or_else(|| panic!("Index out of bounds: {:?}", (i, j))),
            )
        }
    }
}

impl<T> IndexMut<(u32, u32)> for Buffer<T> {
    fn index_mut(&mut self, (i, j): (u32, u32)) -> &mut T {
        self.values
            .get_mut()
            .get_mut(i as usize)
            .and_then(|v| v.get_mut(j as usize))
            .unwrap_or_else(|| panic!("Index out of bounds: {:?}", (i, j)))
    }
}

impl<T> Index<(u16, u32)> for Buffer<T> {
    type Output = T;
    fn index(&self, (i, j): (u16, u32)) -> &T {
        &self[(i as u32, j)]
    }
}

impl<T> IndexMut<(u16, u32)> for Buffer<T> {
    fn index_mut(&mut self, (i, j): (u16, u32)) -> &mut T {
        &mut self[(i as u32, j)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_buffer() {
        let mut buffer = Buffer::new();
        for i in 0..10 {
            buffer.push(i);
        }
        buffer.truncate(6);
        assert_eq!(buffer.total_len(), 6);
        buffer.truncate(2);
        assert_eq!(buffer.total_len(), 2);
    }
}
