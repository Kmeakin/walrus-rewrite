use crate::{hir::Var, ty::Type};

macro_rules! builtins {
    (
        $(
            $id:ident {
                name: $name:expr,
                kind: $kind:expr,
                ty: $ty:expr,
            }
        ),*
    ) => {
        #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
        pub enum Builtin {
            $($id),*
        }

        impl Builtin {
            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$id => $name),*
                }
            }

            pub fn kind(self) -> BuiltinKind {
                match self {
                    $(Self::$id => $kind),*
                }
            }

            pub fn ty(self) -> Type {
                match self {
                    $(Self::$id => $ty),*
                }
            }

            pub fn lookup(var:&Var) -> Option<Builtin>{
                let builtin = match var.as_str() {
                    $($name => Builtin::$id),*,
                    _ => return None,
                };
                Some(builtin)
            }

            pub fn all() -> &'static [Self] {
                &[$(Self::$id),*]
            }
        }
    };
}

builtins! {
    Bool {
        name: "Bool",
        kind: BuiltinKind::Type,
        ty: Type::BOOL,
    },
    Int {
        name: "Int",
        kind: BuiltinKind::Type,
        ty: Type::INT,
    },
    Float {
        name: "Float",
        kind: BuiltinKind::Type,
        ty: Type::FLOAT,
    },
    Char {
        name: "Char",
        kind: BuiltinKind::Type,
        ty: Type::CHAR,
    },
    Never {
        name: "Never",
        kind: BuiltinKind::Type,
        ty: Type::NEVER,
    },
    Exit {
        name: "exit",
        kind: BuiltinKind::Value,
        ty: Type::function(vec![Type::INT], Type::NEVER),
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum BuiltinKind {
    Type,
    Value,
}
