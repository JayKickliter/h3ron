use pyo3::prelude::*;

use h3ron::collections as h3c;
use h3ron_h3_sys::H3Index;
use numpy::{PyArray1, IntoPyArray};
use crate::util::validate_h3_resolution;

#[pyclass]
pub struct H3CompactedVec {
    pub(crate) inner: h3c::H3CompactedVec
}

#[pymethods]
impl H3CompactedVec {

    fn len(&self) -> PyResult<usize> {
        Ok(self.inner.len())
    }

    fn len_resolutions(&self) -> PyResult<Vec<usize>> {
        Ok(self.inner.len_resolutions())
    }

    #[getter]
    fn get_is_empty(&self) -> PyResult<bool> {
        Ok(self.inner.is_empty())
    }

    /// check if the stack contains the index or any of its parents
    ///
    /// This function is pretty inefficient.
    fn contains(&self, h3index: u64) -> PyResult<bool> {
        Ok(self.inner.contains(h3index))
    }

    fn compacted_indexes(&self) -> PyResult<Py<PyArray1<u64>>> {
        let indexes: Vec<_> = self.inner.iter_compacted_indexes().collect();
        return_h3indexes_array(indexes)
    }

    fn compacted_indexes_at_resolution(&self, h3_resolution: u8) -> PyResult<Py<PyArray1<u64>>> {
        validate_h3_resolution(h3_resolution)?;
        let indexes = self.inner.get_compacted_indexes_at_resolution(h3_resolution).to_vec();
        return_h3indexes_array(indexes)
    }

    fn uncompacted_indexes_at_resolution(&self, h3_resolution: u8) -> PyResult<Py<PyArray1<u64>>> {
        validate_h3_resolution(h3_resolution)?;
        let indexes = self.inner.iter_uncompacted_indexes(h3_resolution).collect();
        return_h3indexes_array(indexes)
    }
}

fn return_h3indexes_array(h3indexes: Vec<H3Index>) -> PyResult<Py<PyArray1<u64>>> {
    let gil = Python::acquire_gil();
    let py = gil.python();
    Ok(h3indexes.into_pyarray(py).to_owned())
}