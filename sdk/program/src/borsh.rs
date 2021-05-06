#![allow(clippy::integer_arithmetic)]
//! Borsh utils
use {
    borsh::{
        maybestd::io::{Error, Write},
        schema::{BorshSchema, Declaration, Definition, Fields},
        BorshDeserialize, BorshSerialize,
    },
    std::collections::HashMap,
};

/// Get packed length for the given BorchSchema Declaration
fn get_declaration_packed_len(
    declaration: &str,
    definitions: &HashMap<Declaration, Definition>,
) -> usize {
    match definitions.get(declaration) {
        Some(Definition::Array { length, elements }) => {
            *length as usize * get_declaration_packed_len(elements, definitions)
        }
        Some(Definition::Enum { variants }) => {
            1 + variants
                .iter()
                .map(|(_, declaration)| get_declaration_packed_len(declaration, definitions))
                .max()
                .unwrap_or(0)
        }
        Some(Definition::Struct { fields }) => match fields {
            Fields::NamedFields(named_fields) => named_fields
                .iter()
                .map(|(_, declaration)| get_declaration_packed_len(declaration, definitions))
                .sum(),
            Fields::UnnamedFields(declarations) => declarations
                .iter()
                .map(|declaration| get_declaration_packed_len(declaration, definitions))
                .sum(),
            Fields::Empty => 0,
        },
        Some(Definition::Sequence {
            elements: _elements,
        }) => panic!("Missing support for Definition::Sequence"),
        Some(Definition::Tuple { elements }) => elements
            .iter()
            .map(|element| get_declaration_packed_len(element, definitions))
            .sum(),
        None => match declaration {
            "u8" | "i8" => 1,
            "u16" | "i16" => 2,
            "u32" | "i32" => 4,
            "u64" | "i64" => 8,
            "u128" | "i128" => 16,
            "nil" => 0,
            _ => panic!("Missing primitive type: {}", declaration),
        },
    }
}

/// Get the worst-case packed length for the given BorshSchema
pub fn get_packed_len<S: BorshSchema>() -> usize {
    let schema_container = S::schema_container();
    get_declaration_packed_len(&schema_container.declaration, &schema_container.definitions)
}

/// Deserializes without checking that the entire slice has been consumed
///
/// Normally, `try_from_slice` checks the length of the final slice to ensure
/// that the deserialization uses up all of the bytes in the slice.
///
/// Note that there is a potential issue with this function. Any buffer greater than
/// or equal to the expected size will properly deserialize. For example, if the
/// user passes a buffer destined for a different type, the error won't get caught
/// as easily.
pub fn try_from_slice_unchecked<T: BorshDeserialize>(data: &[u8]) -> Result<T, Error> {
    let mut data_mut = data;
    let result = T::deserialize(&mut data_mut)?;
    Ok(result)
}

/// Helper struct which to count how much data would be written during serialization
#[derive(Default)]
struct WriteCounter {
    count: usize,
}

impl Write for WriteCounter {
    fn write(&mut self, data: &[u8]) -> Result<usize, Error> {
        let amount = data.len();
        self.count += amount;
        Ok(amount)
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Get the packed length for the serialized form of this object instance.
///
/// Useful when working with instances of types that contain a variable-length
/// sequence, such as a Vec or HashMap.  Since it is impossible to know the packed
/// length only from the type's schema, this can be used when an instance already
/// exists, to figure out how much space to allocate in an account.
pub fn get_instance_packed_len<T: BorshSerialize>(instance: &T) -> Result<usize, Error> {
    let mut counter = WriteCounter::default();
    instance.serialize(&mut counter)?;
    Ok(counter.count)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        borsh::{maybestd::io::ErrorKind, BorshSchema, BorshSerialize},
        std::mem::size_of,
    };

    #[derive(BorshSerialize, BorshDeserialize, BorshSchema)]
    enum TestEnum {
        NoValue,
        Value(u32),
        StructValue {
            #[allow(dead_code)]
            number: u64,
            #[allow(dead_code)]
            array: [u8; 8],
        },
    }

    #[derive(BorshSerialize, BorshDeserialize, BorshSchema)]
    struct TestStruct {
        pub array: [u64; 16],
        pub number_u128: u128,
        pub number_u32: u32,
        pub tuple: (u8, u16),
        pub enumeration: TestEnum,
    }

    #[derive(Debug, PartialEq, BorshSerialize, BorshDeserialize, BorshSchema)]
    struct Child {
        pub data: [u8; 64],
    }

    #[derive(Debug, PartialEq, BorshSerialize, BorshDeserialize, BorshSchema)]
    struct Parent {
        pub data: Vec<Child>,
    }

    #[test]
    fn unchecked_deserialization() {
        let data = vec![
            Child { data: [0u8; 64] },
            Child { data: [1u8; 64] },
            Child { data: [2u8; 64] },
        ];
        let parent = Parent { data };

        // exact size, both work
        let mut byte_vec = vec![0u8; 4 + get_packed_len::<Child>() * 3];
        let mut bytes = byte_vec.as_mut_slice();
        parent.serialize(&mut bytes).unwrap();
        let deserialized = Parent::try_from_slice(&byte_vec).unwrap();
        assert_eq!(deserialized, parent);
        let deserialized = try_from_slice_unchecked::<Parent>(&byte_vec).unwrap();
        assert_eq!(deserialized, parent);

        // too big, only unchecked works
        let mut byte_vec = vec![0u8; 4 + get_packed_len::<Child>() * 10];
        let mut bytes = byte_vec.as_mut_slice();
        parent.serialize(&mut bytes).unwrap();
        let err = Parent::try_from_slice(&byte_vec).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        let deserialized = try_from_slice_unchecked::<Parent>(&byte_vec).unwrap();
        assert_eq!(deserialized, parent);
    }

    #[test]
    fn packed_len() {
        assert_eq!(
            get_packed_len::<TestEnum>(),
            size_of::<u8>() + size_of::<u64>() + size_of::<u8>() * 8
        );
        assert_eq!(
            get_packed_len::<TestStruct>(),
            size_of::<u64>() * 16
                + size_of::<u128>()
                + size_of::<u32>()
                + size_of::<u8>()
                + size_of::<u16>()
                + get_packed_len::<TestEnum>()
        );
    }

    #[test]
    fn instance_packed_len() {
        let data = vec![
            Child { data: [0u8; 64] },
            Child { data: [1u8; 64] },
            Child { data: [2u8; 64] },
            Child { data: [3u8; 64] },
            Child { data: [4u8; 64] },
            Child { data: [5u8; 64] },
        ];
        let parent = Parent { data };
        assert_eq!(
            get_instance_packed_len(&parent).unwrap(),
            4 + parent.data.len() * get_packed_len::<Child>()
        );
    }
}
