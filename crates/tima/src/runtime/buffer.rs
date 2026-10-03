use std::error::Error;
use std::fmt;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub(super) struct BufferStorage(Arc<Vec<u8>>);

impl BufferStorage {
    pub(super) fn new(bytes: Vec<u8>) -> Self {
        Self(Arc::new(bytes))
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn with_bytes<R>(&self, operation: impl FnOnce(&[u8]) -> R) -> R {
        operation(self.0.as_slice())
    }

    pub(super) fn to_vec(&self) -> Vec<u8> {
        self.with_bytes(<[u8]>::to_vec)
    }

    pub(super) fn into_vec(self) -> Vec<u8> {
        Arc::try_unwrap(self.0).unwrap_or_else(|shared| (*shared).clone())
    }
}

impl PartialEq for BufferStorage {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_slice() == other.0.as_slice()
    }
}

impl Eq for BufferStorage {}

/// Immutable outer shaped byte buffer backed by shareable storage.
///
/// Version zero admits one through three dimensions. Elements are bytes, inner
/// dimensions are dense, and `outer_stride` permits padding between slices of
/// the first dimension. Raster interpretation belongs to codec contracts, not
/// to Tima's value model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferValue {
    pub(super) storage: Arc<BufferStorage>,
    pub(super) shape: Arc<[usize]>,
    pub(super) outer_stride: usize,
}

impl BufferValue {
    pub fn new(
        shape: impl Into<Arc<[usize]>>,
        outer_stride: usize,
        bytes: Vec<u8>,
    ) -> Result<Self, BufferLayoutError> {
        let shape = shape.into();
        validate_buffer_layout(&shape, outer_stride, bytes.len())?;
        Ok(Self {
            storage: Arc::new(BufferStorage::new(bytes)),
            shape,
            outer_stride,
        })
    }

    pub fn shape(&self) -> &[usize] {
        &self.shape
    }

    pub fn outer_stride(&self) -> usize {
        self.outer_stride
    }

    pub fn byte_len(&self) -> usize {
        self.storage.len()
    }

    pub fn with_bytes<R>(&self, operation: impl FnOnce(&[u8]) -> R) -> R {
        self.storage.with_bytes(operation)
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.storage.0.as_slice()
    }

    pub(crate) fn into_parts(self) -> (Vec<u8>, Arc<[usize]>, usize) {
        let storage = match Arc::try_unwrap(self.storage) {
            Ok(storage) => storage.into_vec(),
            Err(shared) => shared.to_vec(),
        };
        (storage, self.shape, self.outer_stride)
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.storage.to_vec()
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.to_vec()
    }

    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.storage, &other.storage)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferLayoutError {
    message: String,
}

impl fmt::Display for BufferLayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for BufferLayoutError {}

fn validate_buffer_layout(
    shape: &[usize],
    outer_stride: usize,
    byte_len: usize,
) -> Result<(), BufferLayoutError> {
    if !(1..=3).contains(&shape.len()) {
        return Err(BufferLayoutError {
            message: "buffer rank must be from 1 through 3".to_owned(),
        });
    }
    let dense_inner = shape[1..]
        .iter()
        .try_fold(1_usize, |product, dimension| {
            product.checked_mul(*dimension)
        })
        .ok_or_else(|| BufferLayoutError {
            message: "buffer inner dimensions overflow usize".to_owned(),
        })?;
    let (outer, minimum_stride) = if shape.len() == 1 {
        (1, shape[0])
    } else {
        (shape[0], dense_inner)
    };
    if outer_stride < minimum_stride {
        return Err(BufferLayoutError {
            message: format!(
                "buffer outer stride {outer_stride} is smaller than dense inner length {minimum_stride}"
            ),
        });
    }
    if shape.len() == 1 && outer_stride != minimum_stride {
        return Err(BufferLayoutError {
            message: "rank-1 buffer outer stride must equal its length".to_owned(),
        });
    }
    let expected = outer
        .checked_mul(outer_stride)
        .ok_or_else(|| BufferLayoutError {
            message: "buffer byte length overflows usize".to_owned(),
        })?;
    if byte_len != expected {
        return Err(BufferLayoutError {
            message: format!(
                "buffer storage has {byte_len} bytes, but shape {shape:?} and outer stride {outer_stride} require {expected}"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::BufferValue;

    #[test]
    fn validates_outer_buffer_layouts() {
        let error = BufferValue::new(Vec::<usize>::new(), 0, Vec::new()).unwrap_err();
        assert!(error.to_string().contains("rank must be from 1 through 3"));
        let error = BufferValue::new(vec![1, 1, 1, 1], 1, vec![0]).unwrap_err();
        assert!(error.to_string().contains("rank must be from 1 through 3"));

        let error = BufferValue::new(vec![2, 2], 2, vec![0; 3]).unwrap_err();
        assert!(error.to_string().contains("require 4"));

        let padded = BufferValue::new(vec![1, 2, 4], 10, vec![0; 10]).unwrap();
        assert_eq!(padded.shape(), &[1, 2, 4]);
        assert_eq!(padded.outer_stride(), 10);
        let error = BufferValue::new(vec![1, 2, 4], 7, vec![0; 7]).unwrap_err();
        assert!(error.to_string().contains("dense inner length 8"));
    }
}
