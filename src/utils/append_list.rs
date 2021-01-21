/// AppendList is a low-level primitive supporting two safe operations:
/// `push`, which appends a node to the list, and `iter` which iterates the list
/// The list cannot be shrunk whilst in use.
use std::sync::atomic::{AtomicPtr, Ordering};
use std::{mem, ptr};

type NodePtr<T> = Option<Box<Node<T>>>;

trait IntoRaw<T> {
    fn into_raw(self) -> *mut T;
}

impl<T> IntoRaw<Node<T>> for NodePtr<T> {
    fn into_raw(self) -> *mut Node<T> {
        match self {
            Some(b) => Box::into_raw(b),
            None => ptr::null_mut(),
        }
    }
}

#[derive(Debug)]
struct Node<T> {
    value: T,
    next: AppendList<T>,
}

#[derive(Debug)]
pub struct AppendList<T>(AtomicPtr<Node<T>>);

impl<T> AppendList<T> {
    unsafe fn from_raw(ptr: *mut Node<T>) -> NodePtr<T> {
        if ptr.is_null() {
            None
        } else {
            Some(Box::from_raw(ptr))
        }
    }

    fn new_internal(node: NodePtr<T>) -> Self {
        AppendList(AtomicPtr::new(node.into_raw()))
    }

    pub fn new() -> Self {
        Self::new_internal(None)
    }

    pub fn append(&self, value: T) {
        self.append_list(AppendList::new_internal(Some(Box::new(Node {
            value,
            next: AppendList::new(),
        }))));
    }

    unsafe fn append_ptr(&self, p: *mut Node<T>) {
        loop {
            match self.0.compare_exchange_weak(
                ptr::null_mut(),
                p,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(head) => {
                    if !head.is_null() {
                        return (*head).next.append_ptr(p);
                    }
                }
            }
        }
    }

    pub fn append_list(&self, other: AppendList<T>) {
        let p = other.0.load(Ordering::Acquire);
        mem::forget(other);
        unsafe { self.append_ptr(p) };
    }

    pub fn iter(&self) -> AppendListIterator<T> {
        AppendListIterator(&self.0)
    }

    /// Returns true if the AppendList contains no data
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    /// get the length of the list, this is O(n)
    pub fn len(&self) -> usize {
        let mut l = 0;
        for _ in self.iter() {
            l += 1;
        }
        l
    }
}

impl<'a, T> IntoIterator for &'a AppendList<T> {
    type Item = &'a T;
    type IntoIter = AppendListIterator<'a, T>;

    fn into_iter(self) -> AppendListIterator<'a, T> {
        self.iter()
    }
}

impl<T> ::std::iter::FromIterator<T> for AppendList<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let l = AppendList::new();
        for i in iter {
            l.append(i);
        }
        l
    }
}

impl<T> Default for AppendList<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for AppendList<T> {
    fn drop(&mut self) {
        unsafe { Self::from_raw(mem::replace(self.0.get_mut(), ptr::null_mut())) };
    }
}

#[derive(Debug)]
pub struct AppendListIterator<'a, T: 'a>(&'a AtomicPtr<Node<T>>);

impl<'a, T: 'a> Iterator for AppendListIterator<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        let p = self.0.load(Ordering::Acquire);
        if p.is_null() {
            None
        } else {
            unsafe {
                self.0 = &(*p).next.0;
                Some(&(*p).value)
            }
        }
    }
}
