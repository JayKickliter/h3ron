use std::collections::HashMap;
use std::hash::Hash;

use geo_types::{Coordinate, Rect};
use log::debug;
use ndarray::{
    ArrayView2,
    Axis,
    parallel::prelude::*,
};

use h3::{
    index::Index,
    polyfill,
    stack::H3IndexStack,
};

use crate::{
    error::Error,
    transform::Transform,
};

// already imported by ndarray::parallel::prelude
//use rayon::prelude::*;

/// the order of the axis in the two-dimensional array
pub enum AxisOrder {
    XY,

    /// the order used by github.com/georust/gdal (ndarray feature gate)
    YX
}

impl AxisOrder {
    pub fn x_axis(&self) -> usize {
        match self {
            Self::XY => 0,
            Self::YX => 1,
        }
    }

    pub fn y_axis(&self) -> usize {
        match self {
            Self::XY => 1,
            Self::YX => 0,
        }
    }
}

fn find_continuous_chunks_along_axis<T>(a: &ArrayView2<T>, axis: usize, nodata_value: &T) -> Vec<(usize, usize)> where T: Sized + PartialEq {
    let mut chunks = Vec::new();
    let mut current_chunk_start: Option<usize> = None;

    for (r0pos, r0) in a.axis_iter(Axis(axis)).enumerate() {
        if r0.iter().any(|v| v != nodata_value) {
            if current_chunk_start.is_none() {
                current_chunk_start = Some(r0pos);
            }
        } else if let Some(begin) = current_chunk_start {
            chunks.push((begin, r0pos - 1));
            current_chunk_start = None;
        }
    }
    if let Some(begin) = current_chunk_start {
        chunks.push((begin, a.shape()[axis] - 1));
    }
    chunks
}

/// find all boxes in the array where there are any values except the nodata_value
///
/// this implementation is far from perfect and often recognizes multiple smaller
/// clusters as one as its based on completely empty columns and rows, but it is probably
/// sufficient for the purpose to reduce the number of hexagons
/// to be generated when dealing with fragmented/sparse datasets.
fn find_boxes_containing_data<T>(a: &ArrayView2<T>, nodata_value: &T, axis_order: &AxisOrder) -> Vec<Rect<usize>> where T: Sized + PartialEq {
    let mut boxes = Vec::new();

    for chunk_x_raw_indexes in find_continuous_chunks_along_axis(a, axis_order.x_axis(), nodata_value) {
        let sv = {
            let x_raw_range = chunk_x_raw_indexes.0..=chunk_x_raw_indexes.1;
            match axis_order {
                AxisOrder::XY => a.slice(s![x_raw_range, ..]),
                AxisOrder::YX => a.slice(s![.., x_raw_range]),
            }
        };
        for chunks_y_raw_indexes in find_continuous_chunks_along_axis(&sv, axis_order.y_axis(), nodata_value) {
            let sv2 = {
                let x_raw_range = 0..=(chunk_x_raw_indexes.1-chunk_x_raw_indexes.0);
                let y_raw_range = chunks_y_raw_indexes.0..=chunks_y_raw_indexes.1;
                match axis_order {
                    AxisOrder::XY => sv.slice(s![x_raw_range, y_raw_range]),
                    AxisOrder::YX => sv.slice(s![y_raw_range, x_raw_range]),
                }
            };

            // one more iteration along axis 0 to get the specific range for that axis 1 range
            for chunks_x_indexes in find_continuous_chunks_along_axis(&sv2, axis_order.x_axis(), nodata_value) {
                boxes.push(Rect::new(
                    Coordinate {
                        x: chunks_x_indexes.0 + chunk_x_raw_indexes.0,
                        y: chunks_y_raw_indexes.0,
                    },
                    Coordinate {
                        x: chunks_x_indexes.1 + chunk_x_raw_indexes.0,
                        y: chunks_y_raw_indexes.1,
                    },
                ))
            }
        }
    }
    boxes
}

pub struct H3Converter<'a, T> where T: Sized + PartialEq + Sync + Eq + Hash {
    arr: &'a ArrayView2<'a, T>,
    nodata_value: &'a T,
    transform: &'a Transform,
    axis_order: AxisOrder,
}

impl<'a, T> H3Converter<'a, T> where T: Sized + PartialEq + Sync + Eq + Hash {
    pub fn new(arr: &'a ArrayView2<'a, T>, nodata_value: &'a T, transform: &'a Transform, axis_order: AxisOrder) -> Self {
        Self {
            arr,
            nodata_value,
            transform,
            axis_order,
        }
    }

    fn rects_with_data(&self, rect_size: usize) -> Vec<Rect<f64>> {
        self.arr.axis_chunks_iter(Axis(self.axis_order.x_axis()), rect_size)
            .into_par_iter() // requires T to be Sync
            .enumerate()
            .map(|(axis_x_chunk_i, axis_x_chunk)| {
                let mut rects = Vec::new();
                for chunk_x_rect in find_boxes_containing_data(&axis_x_chunk, self.nodata_value, &self.axis_order) {
                    let offset_x = (axis_x_chunk_i * rect_size) + chunk_x_rect.min().x;
                    let chunk_rect_view = {
                        let x_range = chunk_x_rect.min().x..chunk_x_rect.max().x;
                        let y_range = chunk_x_rect.min().y..chunk_x_rect.max().y;
                        match self.axis_order {
                            AxisOrder::XY => axis_x_chunk.slice(s![x_range, y_range]),
                            AxisOrder::YX => axis_x_chunk.slice(s![y_range, x_range]),
                        }
                    };
                    chunk_rect_view.axis_chunks_iter(Axis(self.axis_order.y_axis()), rect_size)
                        .enumerate()
                        .for_each(|(c_i, c)| {
                            let offset_y = (c_i * rect_size) + chunk_x_rect.min().y;

                            // the window in array coordinates
                            let window = Rect::new(
                                Coordinate {
                                    x: offset_x as f64,
                                    y: offset_y as f64,
                                },
                                // add one to the max coordinate to include the whole last pixel
                                Coordinate {
                                    x: (offset_x + c.shape()[self.axis_order.x_axis()] + 1) as f64,
                                    y: (offset_y + c.shape()[self.axis_order.y_axis()] + 1) as f64,
                                },
                            );
                            rects.push(window)
                        })
                }
                rects
            }).flatten().collect()
    }

    fn finalize_chunk_map(&self, chunk_map: &mut HashMap<&T, H3IndexStack>, compact: bool) {
        chunk_map.iter_mut()
            .for_each(|(_value, index_stack)| {
                if compact {
                    index_stack.compact();
                } else {
                    index_stack.dedup();
                }
            });
    }

    pub fn to_h3(&self, h3_resolution: u8, compact: bool) -> Result<HashMap<&'a T, H3IndexStack>, Error> {
        let inverse_transform = self.transform.invert()?;

        let rects = self.rects_with_data(250);
        let n_rects = rects.len();
        debug!("to_h3: found {} rects containing non-nodata values", n_rects);

        let mut chunk_h3_maps = rects
            .into_par_iter()
            .enumerate()
            .map(|(array_window_i, array_window)| {
                debug!("to_h3: rect {}/{} with size {} x {}", array_window_i, n_rects, array_window.width(), array_window.height());

                // the window in geographical coordinates
                let window_box = self.transform * &array_window;
                //dbg!(window_box);

                let mut chunk_h3_map = HashMap::<&T, H3IndexStack>::new();
                let h3indexes = polyfill(&window_box.to_polygon(), h3_resolution);
                //println!("num h3indexes: {}", h3indexes.len());
                for h3index in h3indexes {
                    // find the array element for the coordinate of the h3 index
                    let arr_coord = {
                        let transformed = &inverse_transform * &Index::from(h3index).coordinate();
                        /*
                        Coordinate {
                            x: transformed.x.floor() as usize,
                            y: transformed.y.floor() as usize,
                        }

                         */
                        match self.axis_order {
                            AxisOrder::XY => [transformed.x.floor() as usize, transformed.y.floor() as usize],
                            AxisOrder::YX => [transformed.y.floor() as usize, transformed.x.floor() as usize],
                        }
                    };
                    if let Some(value) = self.arr.get(arr_coord) {
                        if value != self.nodata_value {
                            chunk_h3_map.entry(value)
                                .or_insert_with(H3IndexStack::new)
                                .add_indexes(&[h3index], false);
                        }
                    }
                }

                // do an early compacting to free a bit of memory
                self.finalize_chunk_map(&mut chunk_h3_map, compact);

                chunk_h3_map
            })
            .collect::<Vec<_>>();

        // combine the results from all chunks
        let mut h3_map = HashMap::new();
        for mut chunk_h3_map in chunk_h3_maps.drain(..) {
            for (value, mut index_stack) in chunk_h3_map.drain() {
                h3_map.entry(value)
                    .or_insert_with(H3IndexStack::new)
                    .append(&mut index_stack, false);
            }
        }

        self.finalize_chunk_map(&mut h3_map, compact);
        Ok(h3_map)
    }
}

#[cfg(test)]
mod tests {
    use crate::array::find_boxes_containing_data;
    use crate::AxisOrder;

    #[test]
    fn test_find_boxes_containing_data() {
        let arr = array![
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
            [0, 1, 1, 0, 0, 0, 0, 1, 1, 1, 0, 0],
            [0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 0, 0],
            [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 1],
            [0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 1, 1],
        ];
        let mut arr_copy = arr.clone();

        let n_elements = arr_copy.shape()[0] * arr_copy.shape()[1];
        let mut n_elements_in_boxes = 0;

        for rect in find_boxes_containing_data(&arr.view(), &0, &AxisOrder::XY) {
            n_elements_in_boxes += (rect.max().x - rect.min().x + 1) * (rect.max().y - rect.min().y + 1);

            //dbg!(rect);
            for x in rect.min().x..=rect.max().x {
                for y in rect.min().y..=rect.max().y {
                    arr_copy[(x, y)] = 0;
                }
            }
        }
        //dbg!(n_elements);
        //dbg!(n_elements_in_boxes);

        // there should be far less indexes to visit now
        assert!(n_elements_in_boxes < (n_elements / 2));

        // all elements should have been removed
        assert_eq!(arr_copy.sum(), 0);
    }
}
