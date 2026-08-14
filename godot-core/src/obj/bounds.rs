/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Different ways how bounds of a `GodotClass` can be checked.
//!
//! This module contains multiple traits that can be used to check the characteristics of a `GodotClass` type:
//!
//! 1. [`Declarer`] tells you whether the class is provided by the engine or user-defined.
//!    - [`DeclEngine`] is used for all classes provided by the engine (e.g. `Node3D`).
//!    - [`DeclUser`] is used for all classes defined by the user, typically through `#[derive(GodotClass)]`.<br><br>
//!
//! 2. [`Memory`] is used to check the memory strategy of the **static** type.
//!
//!    This is useful when you operate on associated functions of `Gd<T>` or `T`, e.g. for construction.
//!    - [`MemRefCounted`] is used for `RefCounted` classes and derived.
//!    - [`MemManual`] is used for `Object` and all inherited classes, which are not `RefCounted` (e.g. `Node`).<br><br>
//!
// FIXME excluded because broken; see below.
// 3. [`DynMemory`] is used to check the memory strategy of the **dynamic** type.
//
//    When you operate on methods of `T` or `Gd<T>` and are interested in instances, you can use this.
//    Most of the time, this is not what you want -- just use `Memory` if you want to know if a type is manually managed or ref-counted.
//    - [`MemRefCounted`] is used for `RefCounted` classes and derived. These are **always** reference-counted.
//    - [`MemManual`] is used instances inheriting `Object`, which are not `RefCounted` (e.g. `Node`). Excludes `Object` itself. These are
//      **always** manually managed.
//    - [`MemDynamic`] is used for `Object` instances. `Gd<Object>` can point to objects of any possible class, so whether we are dealing with
//      a ref-counted or manually-managed object is determined only at runtime.
//!
//!
//! # Example
//!
//! Declare a custom smart pointer which wraps `Gd<T>` pointers, but only accepts `T` objects that are manually managed.
//! ```
//! use godot::prelude::*;
//! use godot::obj::{bounds, Bounds};
//!
//! struct MyGd<T>
//! where T: GodotClass + Bounds<Memory = bounds::MemManual>
//! {
//!    inner: Gd<T>,
//! }
//! ```
//!
// Note that depending on if you want to exclude `Object`, you should use `DynMemory` instead of `Memory`.

use private::Sealed;

use crate::obj::cap::GodotDefault;
use crate::obj::{Bounds, Gd, GodotClass, RawGd};
use crate::storage::{InstanceCache, Storage};
use crate::{out, sys};

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Sealed trait

pub(super) mod private {
    use super::{Declarer, DynMemory, Exportable, Memory};

    // Bounds trait declared here for code locality; re-exported in crate::obj.

    /// Library-implemented trait to check bounds on `GodotClass` types.
    ///
    /// See [`bounds`](crate::obj::bounds) module for how to use this for bounds checking.
    ///
    /// # No manual `impl`
    ///
    /// <div class="warning">
    /// <strong>Never</strong> implement this trait manually.
    /// </div>
    ///
    /// Most of the time, this trait is covered by [`#[derive(GodotClass)]`](../register/derive.GodotClass.html).
    /// If you implement `GodotClass` manually, use the [`implement_godot_bounds!`][crate::implement_godot_bounds] macro.
    ///
    /// There are two reasons to avoid a handwritten `impl Bounds`:
    /// - The trait is `unsafe` and it is very easy to get internal bounds wrong. This will lead to immediate UB.
    /// - Apart from the documented members, the trait may have undocumented items that may be broken at any time and stand under no SemVer
    ///   guarantees.
    ///
    /// # Safety
    ///
    /// Internal. The library implements this trait and ensures safety.
    pub unsafe trait Bounds {
        /// Defines the memory strategy of the static type.
        type Memory: Memory;

        // FIXME: this is broken as a bound: one cannot use T: Bounds<DynMemory = MemRefCounted> to include Object AND RefCounted,
        // since Object itself has DynMemory = MemDynamic. Needs to either use traits like in gdnative, or more types to account for
        // different combinations (as only positive ones can be expressed, not T: Bounds<Memory != MemManual>).
        #[doc(hidden)]
        /// Defines the memory strategy of the instance (at runtime).
        type DynMemory: DynMemory;

        /// Whether this class is a core Godot class provided by the engine, or declared by the user as a Rust struct.
        // TODO what about GDScript user classes?
        type Declarer: Declarer;

        /// True if *either* `T: Inherits<Node>` *or* `T: Inherits<Resource>` is fulfilled.
        ///
        /// Enables `#[export]` for those classes.
        #[doc(hidden)]
        type Exportable: Exportable;
    }

    /// Implements [`Bounds`] for a user-defined class.
    ///
    /// This is only necessary if you do not use the proc-macro API.
    ///
    /// Since `Bounds` is a supertrait of [`GodotClass`][crate::obj::GodotClass], you cannot accidentally forget to implement it.
    ///
    /// # Example
    /// ```no_run
    /// use godot::prelude::*;
    /// use godot::obj::bounds::implement_godot_bounds;
    /// use godot::meta::ClassId;
    ///
    /// struct MyClass {}
    ///
    /// impl GodotClass for MyClass {
    ///     type Base = Node;
    ///
    ///     fn class_id() -> ClassId {
    ///         ClassId::new_cached::<MyClass>(|| "MyClass".to_string())
    ///     }
    /// }
    ///
    /// implement_godot_bounds!(MyClass);
    #[macro_export]
    macro_rules! implement_godot_bounds {
        ($UserClass:ty) => {
            // SAFETY: bounds are library-defined, dependent on base. User has no influence in selecting them -> macro is safe.
            unsafe impl $crate::obj::Bounds for $UserClass {
                type Memory = <<$UserClass as $crate::obj::GodotClass>::Base as $crate::obj::Bounds>::Memory;
                type DynMemory = <<$UserClass as $crate::obj::GodotClass>::Base as $crate::obj::Bounds>::DynMemory;
                type Declarer = $crate::obj::bounds::DeclUser;
                type Exportable = <<$UserClass as $crate::obj::GodotClass>::Base as $crate::obj::Bounds>::Exportable;
            }
        };
    }

    pub trait Sealed {}
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Macro re-exports

pub use crate::implement_godot_bounds;
// ----------------------------------------------------------------------------------------------------------------------------------------------
// Memory bounds

/// Specifies the memory strategy of the static type.
pub trait Memory: Sealed {
    /// True for everything inheriting `RefCounted`, false for `Object` and all other classes.
    #[doc(hidden)]
    const IS_REF_COUNTED: bool;
}

/// Specifies the memory strategy of the dynamic type.
///
/// For `Gd<Object>`, it is determined at runtime whether the instance is manually managed or ref-counted.
#[doc(hidden)]
pub trait DynMemory: Sealed {
    /// Initialize reference counter
    #[doc(hidden)]
    fn maybe_init_ref<T: GodotClass>(obj: &mut RawGd<T>);

    /// If ref-counted, then increment count
    #[doc(hidden)]
    fn maybe_inc_ref<T: GodotClass>(obj: &mut RawGd<T>);

    /// If ref-counted, then decrement count. Returns `true` if the count hit 0 and the object can be
    /// safely freed.
    ///
    /// This behavior can be overriden by a script, making it possible for the function to return `false`
    /// even when the reference count hits 0. This is meant to be used to have a separate reference count
    /// from Godot's internal reference count, or otherwise stop the object from being freed when the
    /// reference count hits 0.
    ///
    /// # Safety
    ///
    /// If this method is used on a [`Gd`] that inherits from [`RefCounted`](crate::classes::RefCounted)
    /// then the reference count must either be incremented before it hits 0, or some [`Gd`] referencing
    /// this object must be forgotten.
    #[doc(hidden)]
    unsafe fn maybe_dec_ref<T: GodotClass>(obj: &mut RawGd<T>) -> bool;

    /// Check if ref-counted, return `None` if information is not available (dynamic and obj dead)
    #[doc(hidden)]
    fn is_ref_counted<T: GodotClass>(obj: &RawGd<T>) -> Option<bool>;

    /// Return the reference count, or `None` if the object is dead or manually managed.
    #[doc(hidden)]
    fn get_ref_count<T: GodotClass>(obj: &RawGd<T>) -> Option<usize>;

    /// Returns `true` if argument and return pointers are passed as `Ref<T>` pointers given this
    /// [`PtrcallType`].
    ///
    /// See [`PtrcallType::Virtual`] for information about `Ref<T>` objects.
    #[doc(hidden)]
    fn pass_as_ref(_call_type: sys::PtrcallType) -> bool {
        false
    }
}

/// Memory managed through Godot reference counter (always present).
/// This is used for `RefCounted` classes and derived.
pub struct MemRefCounted {}
impl Sealed for MemRefCounted {}
impl Memory for MemRefCounted {
    const IS_REF_COUNTED: bool = true;
}
impl DynMemory for MemRefCounted {
    fn maybe_init_ref<T: GodotClass>(obj: &mut RawGd<T>) {
        out!("  MemRefc::init:  {obj:?}");
        if obj.is_null() {
            return;
        }

        // SAFETY: DynMemory=MemRefCounted statically guarantees T inherits RefCounted.
        unsafe {
            obj.with_ref_counted_unchecked(|refc| {
                let success = refc.init_ref();
                assert!(success, "init_ref() failed");
            })
        };
    }

    fn maybe_inc_ref<T: GodotClass>(obj: &mut RawGd<T>) {
        out!("  MemRefc::inc:   {obj:?}");
        if obj.is_null() {
            return;
        }

        // SAFETY: DynMemory=MemRefCounted statically guarantees T inherits RefCounted.
        unsafe {
            obj.with_ref_counted_unchecked(|refc| {
                let success = refc.reference();
                assert!(success, "reference() failed");
            })
        };
    }

    unsafe fn maybe_dec_ref<T: GodotClass>(obj: &mut RawGd<T>) -> bool {
        out!("  MemRefc::dec:   {obj:?}");
        if obj.is_null() {
            return false;
        }

        // SAFETY: DynMemory=MemRefCounted statically guarantees T inherits RefCounted.
        unsafe {
            obj.with_ref_counted_unchecked(|refc| {
                let is_last = refc.unreference();
                out!("  +-- was last={is_last}");
                is_last
            })
        }
    }

    fn is_ref_counted<T: GodotClass>(_obj: &RawGd<T>) -> Option<bool> {
        Some(true)
    }

    fn get_ref_count<T: GodotClass>(obj: &RawGd<T>) -> Option<usize> {
        // SAFETY: DynMemory=MemRefCounted statically guarantees T inherits RefCounted.
        let ref_count =
            unsafe { obj.with_ref_counted_unchecked(|refc| refc.get_reference_count()) };

        // TODO find a safer cast alternative, e.g. num-traits crate with ToPrimitive (Debug) + AsPrimitive (Release).
        Some(ref_count as usize)
    }

    fn pass_as_ref(call_type: sys::PtrcallType) -> bool {
        matches!(call_type, sys::PtrcallType::Virtual)
    }
}

/// Memory managed through Godot reference counter, if present; otherwise manual.
/// This is used only for `Object` classes.
#[doc(hidden)]
pub struct MemDynamic {}
impl MemDynamic {
    /// Check whether dynamic type is ref-counted.
    fn inherits_refcounted<T: GodotClass>(obj: &RawGd<T>) -> bool {
        obj.instance_id_unchecked()
            .is_some_and(|id| id.is_ref_counted())
    }
}
impl Sealed for MemDynamic {}
impl DynMemory for MemDynamic {
    fn maybe_init_ref<T: GodotClass>(obj: &mut RawGd<T>) {
        out!("  MemDyn::init:  {obj:?}");
        if Self::inherits_refcounted(obj) {
            // Will call `RefCounted::init_ref()` which checks for liveness.
            out!("    MemDyn -> MemRefc");
            MemRefCounted::maybe_init_ref(obj)
        } else {
            out!("    MemDyn -> MemManu");
        }
    }

    fn maybe_inc_ref<T: GodotClass>(obj: &mut RawGd<T>) {
        out!("  MemDyn::inc:   {obj:?}");
        if Self::inherits_refcounted(obj) {
            // Will call `RefCounted::reference()` which checks for liveness.
            MemRefCounted::maybe_inc_ref(obj)
        }
    }

    unsafe fn maybe_dec_ref<T: GodotClass>(obj: &mut RawGd<T>) -> bool {
        unsafe {
            out!("  MemDyn::dec:   {obj:?}");
            if obj
                .instance_id_unchecked()
                .is_some_and(|id| id.is_ref_counted())
            {
                // Will call `RefCounted::unreference()` which checks for liveness.
                MemRefCounted::maybe_dec_ref(obj)
            } else {
                false
            }
        }
    }

    fn is_ref_counted<T: GodotClass>(obj: &RawGd<T>) -> Option<bool> {
        // Return `None` if obj is dead
        obj.instance_id_unchecked().map(|id| id.is_ref_counted())
    }

    fn get_ref_count<T: GodotClass>(obj: &RawGd<T>) -> Option<usize> {
        if Self::inherits_refcounted(obj) {
            MemRefCounted::get_ref_count(obj)
        } else {
            None
        }
    }
}

/// No memory management, user responsible for not leaking.
/// This is used for all `Object` derivates, which are not `RefCounted`. `Object` itself is also excluded.
pub struct MemManual {}
impl Sealed for MemManual {}
impl Memory for MemManual {
    const IS_REF_COUNTED: bool = false;
}
impl DynMemory for MemManual {
    fn maybe_init_ref<T: GodotClass>(_obj: &mut RawGd<T>) {}
    fn maybe_inc_ref<T: GodotClass>(_obj: &mut RawGd<T>) {}
    unsafe fn maybe_dec_ref<T: GodotClass>(_obj: &mut RawGd<T>) -> bool {
        false
    }
    fn is_ref_counted<T: GodotClass>(_obj: &RawGd<T>) -> Option<bool> {
        Some(false)
    }
    fn get_ref_count<T: GodotClass>(_obj: &RawGd<T>) -> Option<usize> {
        None
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Declarer bounds

/// Trait that specifies who declares a given `GodotClass`.
pub trait Declarer: Sealed {
    /// The target type of a `Deref` operation on a `Gd<T>`.
    #[doc(hidden)]
    type DerefTarget<T: GodotClass>: GodotClass;

    /// Used as a field in `RawGd`; only set for user-defined classes.
    #[doc(hidden)]
    #[allow(private_bounds)]
    type InstanceCache: InstanceCache;

    /// Check if the object is a user object *and* currently locked by a `bind()` or `bind_mut()` guard.
    ///
    /// # Safety
    /// Object must be alive.
    #[doc(hidden)]
    unsafe fn is_currently_bound<T>(obj: &RawGd<T>) -> bool
    where
        T: GodotClass + Bounds<Declarer = Self>;

    #[doc(hidden)]
    fn create_gd<T>() -> Gd<T>
    where
        T: GodotDefault + Bounds<Declarer = Self>;
}

/// Expresses that a class is declared by the Godot engine.
pub enum DeclEngine {}
impl Sealed for DeclEngine {}
impl Declarer for DeclEngine {
    type DerefTarget<T: GodotClass> = T;
    type InstanceCache = ();

    unsafe fn is_currently_bound<T>(_obj: &RawGd<T>) -> bool
    where
        T: GodotClass + Bounds<Declarer = Self>,
    {
        false
    }

    fn create_gd<T>() -> Gd<T>
    where
        T: GodotDefault + Bounds<Declarer = Self>,
    {
        crate::classes::construct_engine_object()
    }
}

/// Expresses that a class is declared by the user.
pub enum DeclUser {}
impl Sealed for DeclUser {}
impl Declarer for DeclUser {
    type DerefTarget<T: GodotClass> = T::Base;
    type InstanceCache = std::cell::Cell<sys::GDExtensionClassInstancePtr>;

    unsafe fn is_currently_bound<T>(obj: &RawGd<T>) -> bool
    where
        T: GodotClass + Bounds<Declarer = Self>,
    {
        // `storage()` returns `None` for placeholder instances (runtime classes accessed in editor); they hold no Rust binding to be bound.
        // Treat as not currently bound -- callers like `Gd::free()` use this only to detect active `bind()` / `bind_mut()` guards.
        match obj.storage() {
            Some(storage) => storage.is_bound(),
            None => false,
        }
    }

    fn create_gd<T>() -> Gd<T>
    where
        T: GodotDefault + Bounds<Declarer = Self>,
    {
        Gd::default_instance()
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Exportable bounds (still hidden)

#[doc(hidden)]
pub trait Exportable: Sealed {}

#[doc(hidden)]
pub enum Yes {}
impl Sealed for Yes {}
impl Exportable for Yes {}

#[doc(hidden)]
pub enum No {}
impl Sealed for No {}
impl Exportable for No {}

// pub(crate) unsafe fn ffi_cast<T: GodotClass, U: GodotClass>(
//     obj: *mut T,
//     rtti: Option<ObjectRtti>,
// ) -> Option<RawGd<U>> {
//     // `self` may be null when we convert a null-variant into a `Option<Gd<T>>`, since we use `ffi_cast`
//     // in the `ffi_from_variant` conversion function to ensure type-correctness. So the chain would be as follows:
//     // - Variant::nil()
//     // - null RawGd<Object>
//     // - null RawGd<T>
//     // - Option::<Gd<T>>::None
//     if gd_is_null(obj, rtti) {
//         // Null can be cast to anything.
//         // Forgetting a null doesn't do anything, since dropping a null also does nothing.
//         return Some(RawGd::null());
//     }
//
//     // Before Godot API calls, make sure the object is alive (and in Debug mode, of the correct type).
//     // Current design decision: EVERY cast fails on incorrect type, even if target type is correct. This avoids the risk of violated
//     // invariants that leak to the Godot implementation. Also, we do not provide a way to recover from bad types -- this is always
//     // a bug that must be solved by the user.
//     check_rtti(obj, rtti, "ffi_cast");
//
//     let class_tag = unsafe { interface_fn!(classdb_get_class_tag)(U::class_id().string_sys()) };
//     let cast_object_ptr = unsafe { interface_fn!(object_cast_to)(obj_sys(obj), class_tag) };
//
//     // Create weak object, as ownership will be moved and reference-counter stays the same.
//     sys::ptr_then(cast_object_ptr, |ptr| unsafe {
//         RawGd::from_obj_sys_weak(ptr)
//     })
// }
//
// pub(crate) fn check_rtti<T: GodotClass>(
//     obj: *mut T,
//     rtti: Option<ObjectRtti>,
//     method_name: &'static str,
// ) {
//     let call_ctx = CallContext::gd::<T>(method_name);
//
//     let instance_id = check_dynamic_type(obj, rtti, &call_ctx);
//     classes::ensure_object_alive(instance_id, obj_sys(obj), &call_ctx);
// }
//
// pub(crate) fn check_dynamic_type<T: GodotClass>(
//     obj: *mut T,
//     rtti: Option<ObjectRtti>,
//     _call_ctx: &CallContext<'static>,
// ) -> InstanceId {
//     debug_assert!(
//         !gd_is_null(obj, rtti),
//         "{_call_ctx}: cannot call method on null object",
//     );
//
//     // SAFETY: code surrounding RawGd<T> ensures that `self` is non-null; above is just a sanity check against internal bugs.
//     let rtti = unsafe { rtti.unwrap_unchecked() };
//     rtti.check_type::<T>();
//     rtti.instance_id()
// }
//
// pub(crate) fn obj_sys<T: GodotClass>(obj: *mut T) -> sys::GDExtensionObjectPtr {
//     obj as sys::GDExtensionObjectPtr
// }
//
// pub(crate) fn with_ref_counted<T: GodotClass, R>(
//     obj: *mut T,
//     rtti: Option<ObjectRtti>,
//     apply: impl Fn(&mut classes::RefCounted) -> R,
// ) -> R {
//     // Note: this previously called Declarer::scoped_mut() - however, no need to go through bind() for changes in base RefCounted.
//     // Any accesses to user objects (e.g. destruction if refc=0) would bind anyway.
//
//     let tmp = unsafe { ffi_cast::<T, classes::RefCounted>(obj, rtti) };
//     let mut tmp = tmp.expect("object expected to inherit RefCounted");
//     let return_val = apply(tmp.as_target_mut());
//
//     std::mem::forget(tmp); // no ownership transfer
//     return_val
// }
//
// /// Executes a function directly on this object, assuming it is `RefCounted`.
// ///
// /// Unlike [`try_with_ref_counted`](Self::try_with_ref_counted), this does **not** check the type at runtime.
// ///
// /// # Safety
// /// Caller must guarantee that `T` (statically) inherits from `RefCounted`.
// pub(crate) unsafe fn with_ref_counted_unchecked<T: GodotClass, R>(
//     obj: *mut T,
//     rtti: Option<ObjectRtti>,
//     apply: impl FnOnce(&mut classes::RefCounted) -> R,
// ) -> R {
//     let cached_rtti = rtti.map(|rtti| ObjectRtti::of::<classes::RefCounted>(rtti.instance_id()));
//
//     // Note: caller guarantees T: Inherits<RefCounted>. `ManuallyDrop` keeps the refcount balanced when `borrow` goes out of scope.
//     let raw = RawGd::<classes::RefCounted> {
//         memory: MemRefCounted {
//             obj: obj.cast(),
//             cached_rtti,
//         },
//         cached_storage_ptr: InstanceCache::null(),
//     };
//
//     let mut borrow = std::mem::ManuallyDrop::new(raw);
//     apply(borrow.as_target_mut())
// }
//
// pub(crate) fn gd_is_null<T>(obj: *mut T, rtti: Option<ObjectRtti>) -> bool {
//     obj.is_null() || rtti.is_none()
// }
//
// pub fn inherits_refcounted(rtti: Option<ObjectRtti>) -> bool {
//     rtti.map(|rtti| rtti.instance_id())
//         .is_some_and(|id| id.is_ref_counted())
// }
