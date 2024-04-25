use std::{
    cell::{Cell, RefCell},
    ops::DerefMut,
    sync::{Mutex, RwLock},
};

use gc::{force_collect, Gc, GcAble};

#[derive(Debug)]
pub struct ExampleNum {
    n: Mutex<i32>,
    next: Vec<Gc<ExampleNum>>,
}

impl Clone for ExampleNum {
    fn clone(&self) -> Self {
        Self::new(self.n.lock().unwrap().clone(), self.next.clone())
    }
}

impl ExampleNum {
    pub fn new(n: i32, next: Vec<Gc<ExampleNum>>) -> Self {
        Self {
            n: Mutex::new(n),
            next,
        }
    }
    pub fn get(&self) -> i32 {
        *self.n.lock().unwrap()
    }
}

unsafe impl GcAble for ExampleNum {
    unsafe fn mark(&self) {
        self.next.iter().for_each(|gc| gc.mark())
    }

    unsafe fn inc_root_count(&self) {
        self.next.iter().for_each(|gc| gc.inc_root_count())
    }

    unsafe fn dec_root_count(&self) {
        self.next.iter().for_each(|gc| gc.dec_root_count())
    }

    unsafe fn set_not_root(&self) {
        self.next.iter().for_each(|gc| gc.set_not_root())
    }
}

// #[derive(Debug)]
// pub struct ExampleNum {
//     n: Mutex<i32>,
//     next: RwLock<Option<Gc<ExampleNum>>>,
// }

// impl Clone for ExampleNum {
//     fn clone(&self) -> Self {
//         Self::new(
//             self.n.lock().unwrap().clone(),
//             self.next.read().unwrap().clone(),
//         )
//     }
// }

// impl ExampleNum {
//     pub fn new(n: i32, next: Option<Gc<ExampleNum>>) -> Self {
//         Self {
//             n: Mutex::new(n),
//             next: RwLock::new(next),
//         }
//     }
//     pub fn get(&self) -> i32 {
//         *self.n.lock().unwrap()
//     }
// }

// unsafe impl GcAble for ExampleNum {
//     unsafe fn mark(&self) {
//         let next = self.next.read().unwrap();
//         if let Some(next) = &*next {
//             next.mark()
//         }
//     }

//     unsafe fn inc_root_count(&self) {
//         let next = self.next.read().unwrap();
//         if let Some(next) = &*next {
//             next.inc_root_count()
//         }
//     }

//     unsafe fn dec_root_count(&self) {
//         let next = self.next.read().unwrap();
//         if let Some(next) = &*next {
//             next.dec_root_count()
//         }
//     }

//     unsafe fn set_not_root(&mut self) {
//         let mut next = self.next.write().unwrap();
//         if let Some(next) = &mut *next {
//             next.set_not_root()
//         }
//     }
// }

fn main() {
    {
        let tail = Gc::new(ExampleNum::new(2, vec![]));
        let root = ExampleNum::new(0, vec![Gc::new(ExampleNum::new(1, vec![tail.clone()]))]);
        let root = Gc::new(root);

        println!("{root:#?}");
    }

    force_collect();
}
