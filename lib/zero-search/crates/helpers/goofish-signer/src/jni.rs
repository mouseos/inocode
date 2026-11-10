//! JNI environment emulation: object/class/method/field registries,
//! the JNINativeInterface trampoline table, and the Java-side stubs
//! ported from SgHarness.java (round19).
//!
//! Object model: every Java object is a host-side `Obj` in a registry;
//! a jobject handle is a guest address of a per-object cell (like real
//! JNI local/global refs). jclass is an object of kind Class. Method
//! and field IDs are host-assigned integers (guest sees them as opaque
//! pointer-sized values in a reserved range).

use std::collections::HashMap;

pub const JNI_NATIVE_TABLE_ENTRIES: usize = 232;

#[derive(Clone, Debug)]
pub enum ObjKind {
    Class {
        name: String,
    },
    Str(String),
    Int(i32),
    Long(i64),
    Bool(bool),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    PrimArray {
        elem: u8,
        data: Vec<u8>,
    },
    ObjectArray(Vec<u64>), // element handles (cell addresses)
    HashMap(std::collections::HashMap<String, String>),
    StrSet(Vec<String>),
    /// Opaque named object (Context, plugin, File, ...) whose payload
    /// is a string tag/path — mirrors SgHarness's newObject(tag).
    Named {
        class: String,
        tag: String,
    },
    SecException {
        msg: String,
        code: i32,
    },
    Null,
}

#[derive(Clone, Debug)]
pub struct Obj {
    pub kind: ObjKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MethodId {
    pub class: String,
    pub name: String,
    pub sig: String,
    pub is_static: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FieldId {
    pub class: String,
    pub name: String,
    pub sig: String,
    pub is_static: bool,
}

pub struct JniState {
    /// cell address -> object id
    pub cells: HashMap<u64, u32>,
    pub objects: HashMap<u32, Obj>,
    pub next_obj: u32,
    pub next_cell: u64,
    pub classes: HashMap<String, u32>, // class name -> object id (the Class object)
    pub method_ids: HashMap<MethodId, u64>,
    pub method_by_id: HashMap<u64, MethodId>,
    pub field_ids: HashMap<FieldId, u64>,
    pub field_by_id: HashMap<u64, FieldId>,
    pub next_mid: u64,
    /// registered native methods: (class, name, sig) -> guest fn addr
    pub natives: HashMap<(String, String, String), u64>,
    /// pending exception (object id), if any
    pub pending_exception: Option<u32>,
    /// guest address of the JNIEnv struct (points to the table)
    pub env_addr: u64,
    /// guest address of the JavaVM struct
    pub vm_addr: u64,
    /// trampoline address per JNI table index
    pub trampolines: Vec<u64>,
    /// trampoline address per JavaVM (invoke) table index
    pub vm_trampolines: Vec<u64>,
    /// guest addresses holding interned UTF strings for GetStringUTFChars
    pub utf_cache: HashMap<u32, u64>,
    /// pinned array copies: guest buffer addr -> object id
    pub pinned: HashMap<u64, u32>,
    pub log: bool,
}

pub const CELL_BASE: u64 = 0xE300_0000;
pub const METHOD_ID_BASE: u64 = 0xF000_0000;
pub const FIELD_ID_BASE: u64 = 0xF100_0000;

impl JniState {
    pub fn new() -> JniState {
        JniState {
            cells: HashMap::new(),
            objects: HashMap::new(),
            next_obj: 1,
            next_cell: CELL_BASE,
            classes: HashMap::new(),
            method_ids: HashMap::new(),
            method_by_id: HashMap::new(),
            field_ids: HashMap::new(),
            field_by_id: HashMap::new(),
            next_mid: 1,
            natives: HashMap::new(),
            pending_exception: None,
            env_addr: 0,
            vm_addr: 0,
            trampolines: Vec::new(),
            vm_trampolines: Vec::new(),
            utf_cache: HashMap::new(),
            pinned: HashMap::new(),
            log: true,
        }
    }

    pub fn alloc_obj(&mut self, kind: ObjKind) -> u32 {
        let id = self.next_obj;
        self.next_obj += 1;
        self.objects.insert(id, Obj { kind });
        id
    }

    /// Create a new handle cell for an object; returns the cell address
    /// (what guest code sees as jobject/jclass/jstring/...).
    pub fn new_handle(&mut self, obj: u32) -> u64 {
        let cell = self.next_cell;
        self.next_cell += 8;
        self.cells.insert(cell, obj);
        cell
    }

    pub fn handle_of(&mut self, kind: ObjKind) -> u64 {
        let id = self.alloc_obj(kind);
        self.new_handle(id)
    }

    pub fn obj_of_handle(&self, handle: u64) -> Option<&Obj> {
        self.cells.get(&handle).and_then(|id| self.objects.get(id))
    }

    pub fn obj_id_of_handle(&self, handle: u64) -> Option<u32> {
        self.cells.get(&handle).copied()
    }

    pub fn class_handle(&mut self, name: &str) -> u64 {
        if let Some(&id) = self.classes.get(name) {
            return self.new_handle(id);
        }
        let id = self.alloc_obj(ObjKind::Class {
            name: name.to_string(),
        });
        self.classes.insert(name.to_string(), id);
        self.new_handle(id)
    }

    pub fn class_name_of_handle(&self, handle: u64) -> Option<String> {
        match self.obj_of_handle(handle).map(|o| &o.kind) {
            Some(ObjKind::Class { name }) => Some(name.clone()),
            Some(ObjKind::Named { class, .. }) => Some(class.clone()),
            _ => None,
        }
    }

    pub fn method_id(&mut self, class: &str, name: &str, sig: &str, is_static: bool) -> u64 {
        let key = MethodId {
            class: class.to_string(),
            name: name.to_string(),
            sig: sig.to_string(),
            is_static,
        };
        if let Some(&id) = self.method_ids.get(&key) {
            return id;
        }
        let id = METHOD_ID_BASE + self.next_mid * 8;
        self.next_mid += 1;
        self.method_ids.insert(key.clone(), id);
        self.method_by_id.insert(id, key);
        id
    }

    pub fn field_id(&mut self, class: &str, name: &str, sig: &str, is_static: bool) -> u64 {
        let key = FieldId {
            class: class.to_string(),
            name: name.to_string(),
            sig: sig.to_string(),
            is_static,
        };
        if let Some(&id) = self.field_ids.get(&key) {
            return id;
        }
        let id = FIELD_ID_BASE + (self.field_ids.len() as u64 + 1) * 8;
        self.field_ids.insert(key.clone(), id);
        self.field_by_id.insert(id, key);
        id
    }

    pub fn string_obj(&mut self, s: &str) -> u64 {
        self.handle_of(ObjKind::Str(s.to_string()))
    }

    pub fn get_string(&self, handle: u64) -> Option<String> {
        match self.obj_of_handle(handle).map(|o| &o.kind) {
            Some(ObjKind::Str(s)) => Some(s.clone()),
            _ => None,
        }
    }
}

/// JNI function table indices (JNI 1.6 spec order) for the functions
/// we implement explicitly; everything else gets a logging stub that
/// returns 0.
pub mod idx {
    pub const GET_VERSION: usize = 4;
    pub const FIND_CLASS: usize = 6;
    pub const GET_SUPERCLASS: usize = 10;
    pub const IS_ASSIGNABLE_FROM: usize = 11;
    pub const THROW_NEW: usize = 14;
    pub const EXCEPTION_OCCURRED: usize = 15;
    pub const EXCEPTION_CLEAR: usize = 17;
    pub const NEW_GLOBAL_REF: usize = 21;
    pub const DELETE_GLOBAL_REF: usize = 22;
    pub const DELETE_LOCAL_REF: usize = 23;
    pub const IS_SAME_OBJECT: usize = 24;
    pub const NEW_LOCAL_REF: usize = 25;
    pub const ALLOC_OBJECT: usize = 27;
    pub const NEW_OBJECT: usize = 28;
    pub const NEW_OBJECT_V: usize = 29;
    pub const NEW_OBJECT_A: usize = 30;
    pub const GET_OBJECT_CLASS: usize = 31;
    pub const IS_INSTANCE_OF: usize = 32;
    pub const GET_METHOD_ID: usize = 33;
    pub const CALL_OBJECT_METHOD: usize = 34;
    pub const CALL_OBJECT_METHOD_V: usize = 35;
    pub const CALL_OBJECT_METHOD_A: usize = 36;
    pub const CALL_BOOLEAN_METHOD: usize = 37;
    pub const CALL_BOOLEAN_METHOD_V: usize = 38;
    pub const CALL_BOOLEAN_METHOD_A: usize = 39;
    pub const CALL_INT_METHOD: usize = 49;
    pub const CALL_INT_METHOD_V: usize = 50;
    pub const CALL_INT_METHOD_A: usize = 51;
    pub const CALL_LONG_METHOD: usize = 52;
    pub const CALL_LONG_METHOD_V: usize = 53;
    pub const CALL_LONG_METHOD_A: usize = 54;
    pub const CALL_FLOAT_METHOD: usize = 55;
    pub const CALL_FLOAT_METHOD_V: usize = 56;
    pub const CALL_DOUBLE_METHOD: usize = 58;
    pub const CALL_DOUBLE_METHOD_V: usize = 59;
    pub const CALL_VOID_METHOD: usize = 61;
    pub const CALL_VOID_METHOD_V: usize = 62;
    pub const CALL_VOID_METHOD_A: usize = 63;
    pub const GET_FIELD_ID: usize = 94;
    pub const GET_OBJECT_FIELD: usize = 95;
    pub const GET_BOOLEAN_FIELD: usize = 96;
    pub const GET_INT_FIELD: usize = 100;
    pub const GET_LONG_FIELD: usize = 101;
    pub const SET_OBJECT_FIELD: usize = 104;
    pub const SET_INT_FIELD: usize = 109;
    pub const GET_STATIC_METHOD_ID: usize = 113;
    pub const CALL_STATIC_OBJECT_METHOD: usize = 114;
    pub const CALL_STATIC_OBJECT_METHOD_V: usize = 115;
    pub const CALL_STATIC_OBJECT_METHOD_A: usize = 116;
    pub const CALL_STATIC_BOOLEAN_METHOD: usize = 117;
    pub const CALL_STATIC_BOOLEAN_METHOD_V: usize = 118;
    pub const CALL_STATIC_INT_METHOD: usize = 129;
    pub const CALL_STATIC_INT_METHOD_V: usize = 130;
    pub const CALL_STATIC_LONG_METHOD: usize = 132;
    pub const CALL_STATIC_LONG_METHOD_V: usize = 133;
    pub const CALL_STATIC_VOID_METHOD: usize = 141;
    pub const CALL_STATIC_VOID_METHOD_V: usize = 142;
    pub const GET_STATIC_FIELD_ID: usize = 144;
    pub const GET_STATIC_OBJECT_FIELD: usize = 145;
    pub const GET_STATIC_INT_FIELD: usize = 150;
    pub const SET_STATIC_OBJECT_FIELD: usize = 154;
    pub const NEW_STRING: usize = 163;
    pub const GET_STRING_LENGTH: usize = 164;
    pub const GET_STRING_CHARS: usize = 165;
    pub const RELEASE_STRING_CHARS: usize = 166;
    pub const NEW_STRING_UTF: usize = 167;
    pub const GET_STRING_UTF_LENGTH: usize = 168;
    pub const GET_STRING_UTF_CHARS: usize = 169;
    pub const RELEASE_STRING_UTF_CHARS: usize = 170;
    pub const GET_ARRAY_LENGTH: usize = 171;
    pub const NEW_OBJECT_ARRAY: usize = 172;
    pub const GET_OBJECT_ARRAY_ELEMENT: usize = 173;
    pub const SET_OBJECT_ARRAY_ELEMENT: usize = 174;
    pub const NEW_BYTE_ARRAY: usize = 176;
    pub const GET_BYTE_ARRAY_ELEMENTS: usize = 184;
    pub const RELEASE_BYTE_ARRAY_ELEMENTS: usize = 192;
    pub const GET_BYTE_ARRAY_REGION: usize = 201;
    pub const SET_BYTE_ARRAY_REGION: usize = 209;
    pub const REGISTER_NATIVES: usize = 215;
    pub const GET_STRING_UTF_REGION: usize = 221;
    pub const EXCEPTION_CHECK: usize = 228;
    pub const GET_OBJECT_REF_TYPE: usize = 232;
}
