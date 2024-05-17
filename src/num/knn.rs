/// Performs KNN related search queries, classification and regression, and
/// other features/entropies that require KNN to be efficiently computed.
use super::which_distance;
use crate::utils::{list_u32_output, rechunk_to_frame, split_offsets};
use itertools::Itertools;
use kdtree::KdTree;
use ndarray::{s, ArrayView2, Axis};
use polars::prelude::*;
use pyo3_polars::{
    derive::{polars_expr, CallerContext},
    export::polars_core::{
        utils::rayon::prelude::{IntoParallelIterator, ParallelIterator},
        POOL,
    },
};
use serde::Deserialize;

pub fn knn_full_output(_: &[Field]) -> PolarsResult<Field> {
    let idx = Field::new("idx", DataType::List(Box::new(DataType::UInt32)));

    let dist = Field::new("dist", DataType::List(Box::new(DataType::Float64)));
    let v = vec![idx, dist];
    Ok(Field::new("knn_dist", DataType::Struct(v)))
}

#[derive(Deserialize, Debug)]
pub(crate) struct KdtreeKwargs {
    pub(crate) k: usize,
    pub(crate) leaf_size: usize,
    pub(crate) metric: String,
    pub(crate) parallel: bool,
}

#[derive(Deserialize, Debug)]
pub(crate) struct KdtreeRadiusKwargs {
    pub(crate) r: f64,
    pub(crate) leaf_size: usize,
    pub(crate) metric: String,
    pub(crate) parallel: bool,
}

#[inline]
pub fn build_standard_kdtree<'a>(
    dim: usize,
    leaf_size: usize,
    data: &'a ArrayView2<f64>,
) -> KdTree<f64, usize, &'a [f64]> {
    // Building the tree
    let mut tree = KdTree::with_capacity(dim, leaf_size);
    for (i, p) in data.axis_iter(Axis(0)).enumerate() {
        // C order makes sure rows are contiguous. If error, then ignore the addition of that row
        match tree.add(p.to_slice().unwrap(), i) {
            Ok(_) => {}
            Err(_) => {}
        }
    }
    tree
}

#[polars_expr(output_type_func=list_u32_output)]
fn pl_knn_ptwise(
    inputs: &[Series],
    context: CallerContext,
    kwargs: KdtreeKwargs,
) -> PolarsResult<Series> {
    // Set up params
    let mut inputs_offset = 0;

    let id = inputs[inputs_offset].u32()?;
    let id = id.rechunk();
    let id = id.cont_slice()?;
    inputs_offset += 1;

    let filter = if let Ok(skip_index) = inputs[inputs_offset].bool() {
        inputs_offset += 1;
        let filter: Vec<_> = skip_index.iter().map(|b| !b.unwrap_or(false)).collect();
        Some(filter)
    } else {
        None
    };
    let filter = filter.as_ref();

    let dim = inputs[inputs_offset..].len();
    if dim == 0 {
        return Err(PolarsError::ComputeError("KNN: No column found.".into()));
    }

    let data = rechunk_to_frame(&inputs[inputs_offset..])?;
    let nrows = data.height();
    let k = kwargs.k;
    let leaf_size = kwargs.leaf_size;
    let parallel = kwargs.parallel;
    let can_parallel = parallel && !context.parallel();

    let dist_func = which_distance(kwargs.metric.as_str(), dim)?;

    // Need to use C order because C order is row-contiguous
    let data = data.to_ndarray::<Float64Type>(IndexOrder::C)?;

    // Building the tree
    let binding = data.view();
    let tree = build_standard_kdtree(dim, leaf_size, &binding);

    let values_capacity = if filter.is_none() { k + 1 } else { 0 };

    // Building output
    let ca = if can_parallel {
        POOL.install(|| {
            let n_threads = POOL.current_num_threads();
            let splits = split_offsets(nrows, n_threads);
            let chunks: Vec<_> = splits
                .into_par_iter()
                .map(|(offset, len)| {
                    let mut builder = ListPrimitiveChunkedBuilder::<UInt32Type>::new(
                        "",
                        len,
                        values_capacity,
                        DataType::UInt32,
                    );
                    let piece = data.slice(s![offset..offset + len, 0..dim]);
                    for (i, p) in piece.axis_iter(Axis(0)).enumerate() {
                        let nearest = if filter.map(|f| f[i]).unwrap_or(true) {
                            let s = p.to_slice().unwrap();
                            tree.nearest(s, k + 1, &dist_func).ok()
                        } else {
                            None
                        };

                        if let Some(v) = nearest {
                            let s = v.into_iter().map(|(_, i)| id[*i]).collect_vec();
                            builder.append_slice(&s);
                        } else {
                            builder.append_null();
                        }
                    }
                    let ca = builder.finish();
                    ca.downcast_iter().cloned().collect::<Vec<_>>()
                })
                .collect();
            ListChunked::from_chunk_iter("knn", chunks.into_iter().flatten())
        })
    } else {
        let mut builder = ListPrimitiveChunkedBuilder::<UInt32Type>::new(
            "",
            id.len(),
            values_capacity,
            DataType::UInt32,
        );

        for (i, p) in data.rows().into_iter().enumerate() {
            let nearest = if filter.map(|f| f[i]).unwrap_or(true) {
                let s = p.to_slice().unwrap(); // C order makes sure rows are contiguous
                tree.nearest(s, k + 1, &dist_func).ok()
            } else {
                None
            };

            if let Some(v) = nearest {
                let sl = v.into_iter().map(|(_, i)| id[*i]).collect_vec();
                builder.append_slice(&sl);
            } else {
                builder.append_null();
            }
        }
        builder.finish()
    };
    // let ca = builder.finish();
    Ok(ca.into_series())
}

#[polars_expr(output_type_func=list_u32_output)]
fn pl_query_radius_ptwise(
    inputs: &[Series],
    context: CallerContext,
    kwargs: KdtreeRadiusKwargs,
) -> PolarsResult<Series> {
    // Set up params
    let id = inputs[0].u32()?;
    let id = id.rechunk();
    let id = id.cont_slice()?;

    let dim = inputs[1..].len();
    let data = rechunk_to_frame(&inputs[1..])?;
    let nrows = data.height();
    let leaf_size = kwargs.leaf_size;
    let parallel = kwargs.parallel;
    let can_parallel = parallel && !context.parallel();
    let radius = kwargs.r;
    let dist_func = which_distance(kwargs.metric.as_str(), dim)?;

    // Need to use C order because C order is row-contiguous
    let data = data.to_ndarray::<Float64Type>(IndexOrder::C)?;

    // Building the tree
    let binding = data.view();
    let tree = build_standard_kdtree(dim, leaf_size, &binding);

    // Building output
    if can_parallel {
        let ca = POOL.install(|| {
            let n_threads = POOL.current_num_threads();
            let splits = split_offsets(nrows, n_threads);
            let chunks: Vec<_> = splits
                .into_par_iter()
                .map(|(offset, len)| {
                    let mut builder = ListPrimitiveChunkedBuilder::<UInt32Type>::new(
                        "",
                        len,
                        8,
                        DataType::UInt32,
                    );
                    let piece = data.slice(s![offset..offset + len, 0..dim]);
                    for p in piece.axis_iter(Axis(0)) {
                        let sl = p.to_slice().unwrap();
                        if let Ok(v) = tree.within(sl, radius, &dist_func) {
                            let mut out = v.into_iter().map(|(_, i)| id[*i]).collect_vec();
                            out.shrink_to_fit();
                            builder.append_slice(&out);
                        } else {
                            builder.append_null();
                        }
                    }
                    let ca = builder.finish();
                    ca.downcast_iter().cloned().collect::<Vec<_>>()
                })
                .collect();
            ListChunked::from_chunk_iter("knn-radius", chunks.into_iter().flatten())
        });
        Ok(ca.into_series())
    } else {
        let mut builder =
            ListPrimitiveChunkedBuilder::<UInt32Type>::new("", id.len(), 16, DataType::UInt32);
        for p in data.rows() {
            let s = p.to_slice().unwrap(); // C order makes sure rows are contiguous
            if let Ok(v) = tree.within(s, radius, &dist_func) {
                let mut out: Vec<u32> = v.into_iter().map(|(_, i)| id[*i]).collect();
                out.shrink_to_fit();
                builder.append_slice(&out);
            } else {
                builder.append_null();
            }
        }
        let ca = builder.finish();
        Ok(ca.into_series())
    }
}

#[polars_expr(output_type_func=knn_full_output)]
fn pl_knn_ptwise_w_dist(
    inputs: &[Series],
    context: CallerContext,
    kwargs: KdtreeKwargs,
) -> PolarsResult<Series> {
    // Set up params
    let mut inputs_offset = 0;

    let id = inputs[inputs_offset].u32()?;
    let id = id.rechunk();
    let id = id.cont_slice().unwrap();
    inputs_offset += 1;

    let filter = if let Ok(skip_index) = inputs[inputs_offset].bool() {
        inputs_offset += 1;
        let filter: Vec<_> = skip_index.iter().map(|b| !b.unwrap_or(false)).collect();
        Some(filter)
    } else {
        None
    };
    let filter = filter.as_ref();

    let dim = inputs[inputs_offset..].len();

    let data = rechunk_to_frame(&inputs[inputs_offset..])?;
    let nrows = data.height();
    let k = kwargs.k;
    let leaf_size = kwargs.leaf_size;
    let parallel = kwargs.parallel;
    let can_parallel = parallel && !context.parallel();
    let dist_func = which_distance(kwargs.metric.as_str(), dim)?;

    // Need to use C order because C order is row-contiguous
    let data = data.to_ndarray::<Float64Type>(IndexOrder::C)?;

    // Building the tree
    let binding = data.view();
    let tree = build_standard_kdtree(dim, leaf_size, &binding);

    let values_capacity = if filter.is_none() { k + 1 } else { 0 };

    //Building output
    if can_parallel {
        POOL.install(|| {
            let n_threads = POOL.current_num_threads();
            let splits = split_offsets(nrows, n_threads);
            let chunks: (Vec<_>, Vec<_>) = splits
                .into_par_iter()
                .map(|(offset, len)| {
                    let mut nn_builder = ListPrimitiveChunkedBuilder::<UInt32Type>::new(
                        "",
                        len,
                        values_capacity,
                        DataType::UInt32,
                    );
                    let mut rr_builder = ListPrimitiveChunkedBuilder::<Float64Type>::new(
                        "",
                        len,
                        values_capacity,
                        DataType::Float64,
                    );
                    let piece = data.slice(s![offset..offset + len, 0..dim]);
                    for (i, p) in piece.axis_iter(Axis(0)).enumerate() {
                        let nearest = if filter.map(|f| f[i + offset]).unwrap_or(true) {
                            let s = p.to_slice().unwrap();
                            tree.nearest(s, k + 1, &dist_func).ok()
                        } else {
                            None
                        };
                        if let Some(v) = nearest {
                            let mut nn: Vec<u32> = Vec::with_capacity(k + 1);
                            let mut rr: Vec<f64> = Vec::with_capacity(k + 1);
                            //.map(|(_, i)| id[*i]).collect_vec();
                            for (r, i) in v.into_iter() {
                                nn.push(id[*i]);
                                rr.push(r);
                            }
                            nn_builder.append_slice(&nn);
                            rr_builder.append_slice(&rr);
                        } else {
                            nn_builder.append_null();
                            rr_builder.append_null();
                        }
                    }
                    let ca_nn = nn_builder.finish();
                    let ca_rr = rr_builder.finish();
                    (
                        ca_nn.downcast_iter().cloned().collect::<Vec<_>>(),
                        ca_rr.downcast_iter().cloned().collect::<Vec<_>>(),
                    )
                })
                .collect();

            let ca_nn = ListChunked::from_chunk_iter("", chunks.0.into_iter().flatten());
            let ca_nn = ca_nn.with_name("idx").into_series();
            let ca_rr = ListChunked::from_chunk_iter("", chunks.1.into_iter().flatten());
            let ca_rr = ca_rr.with_name("dist").into_series();
            let out = StructChunked::new("knn_dist", &[ca_nn, ca_rr])?;
            Ok(out.into_series())
        })
    } else {
        let mut nn_builder = ListPrimitiveChunkedBuilder::<UInt32Type>::new(
            "",
            id.len(),
            values_capacity,
            DataType::UInt32,
        );

        let mut rr_builder = ListPrimitiveChunkedBuilder::<Float64Type>::new(
            "",
            id.len(),
            values_capacity,
            DataType::Float64,
        );
        for (i, p) in data.rows().into_iter().enumerate() {
            let nearest = if filter.map(|f| f[i]).unwrap_or(true) {
                let s = p.to_slice().unwrap();
                tree.nearest(s, k + 1, &dist_func).ok()
            } else {
                None
            };
            if let Some(v) = nearest {
                // By construction, this unwrap is safe
                let mut w_idx: Vec<u32> = Vec::with_capacity(k + 1);
                let mut w_dist: Vec<f64> = Vec::with_capacity(k + 1);
                for (d, i) in v.into_iter() {
                    w_idx.push(id[*i]);
                    w_dist.push(d);
                }
                nn_builder.append_slice(&w_idx);
                rr_builder.append_slice(&w_dist);
            } else {
                nn_builder.append_null();
                rr_builder.append_null();
            }
        }
        let ca_nn = nn_builder.finish();
        let ca_nn = ca_nn.with_name("idx").into_series();
        let ca_rr = rr_builder.finish();
        let ca_rr = ca_rr.with_name("dist").into_series();
        let out = StructChunked::new("knn_dist", &[ca_nn, ca_rr])?;
        Ok(out.into_series())
    }
}

/// Find all the rows that are the k-nearest neighbors to the point given.
/// Note, only k points will be returned as true, because here the point is considered an "outside" point,
/// not a point in the data.
#[polars_expr(output_type=Boolean)]
fn pl_knn_filter(inputs: &[Series], kwargs: KdtreeKwargs) -> PolarsResult<Series> {
    // Check len
    let pt = inputs[0].f64()?;
    let dim = inputs[1..].len();
    if pt.len() != dim {
        return Err(PolarsError::ComputeError(
            "KNN: input point must be the same dimension as the number of columns in `others`."
                .into(),
        ));
    }
    // Set up the point to query
    let binding = pt.rechunk();
    let p = binding.cont_slice()?;
    // Set up params
    let data = rechunk_to_frame(&inputs[1..])?;
    let nrows = data.height();
    let dim = inputs[1..].len();
    let k = kwargs.k;
    let leaf_size = kwargs.leaf_size;
    let dist_func = which_distance(kwargs.metric.as_str(), dim)?;

    // Need to use C order because C order is row-contiguous
    let data = data.to_ndarray::<Float64Type>(IndexOrder::C)?;

    // Building the tree
    let binding = data.view();
    let tree = build_standard_kdtree(dim, leaf_size, &binding);

    // Building the output
    let mut out: Vec<bool> = vec![false; nrows];
    match tree.nearest(p, k, &dist_func) {
        Ok(v) => {
            for (_, i) in v.into_iter() {
                out[*i] = true;
            }
        }
        Err(e) => {
            return Err(PolarsError::ComputeError(
                ("KNN: ".to_owned() + e.to_string().as_str()).into(),
            ));
        }
    }
    Ok(BooleanChunked::from_slice("", &out).into_series())
}

/// Neighbor count query
#[inline]
pub fn query_nb_cnt<F>(
    tree: &KdTree<f64, usize, &[f64]>,
    data: ArrayView2<f64>,
    dist_func: &F,
    r: f64,
    can_parallel: bool,
) -> UInt32Chunked
where
    F: Fn(&[f64], &[f64]) -> f64 + std::marker::Sync,
{
    let nrows = data.shape()[0];
    let dim = data.shape()[1];
    if can_parallel {
        let n_threads = POOL.current_num_threads();
        let splits = split_offsets(nrows, n_threads);
        let chunks_iter = splits.into_par_iter().map(|(offset, len)| {
            let piece = data.slice(s![offset..offset + len, 0..dim]);
            let out = piece.axis_iter(Axis(0)).map(|p| {
                let sl = p.to_slice().unwrap();
                tree.within_count(sl, r, &dist_func)
                    .map_or(None, |u| Some(u as u32))
            });
            let ca = UInt32Chunked::from_iter_options("", out);
            ca.downcast_iter().cloned().collect::<Vec<_>>()
        });
        let chunks = POOL.install(|| chunks_iter.collect::<Vec<_>>());
        UInt32Chunked::from_chunk_iter("cnt", chunks.into_iter().flatten())
    } else {
        let mut builder: PrimitiveChunkedBuilder<UInt32Type> =
            PrimitiveChunkedBuilder::new("", nrows);
        data.axis_iter(Axis(0)).for_each(|pt| {
            let s = pt.to_slice().unwrap(); // C order makes sure rows are contiguous
            builder.append_option({
                tree.within_count(s, r, &dist_func)
                    .map_or(None, |u| Some(u as u32))
            });
        });
        builder.finish()
    }
}

/// For every point in this dataframe, find the number of neighbors within radius r
/// The point itself is always considered as a neighbor to itself.
#[polars_expr(output_type=UInt32)]
fn pl_nb_cnt(
    inputs: &[Series],
    context: CallerContext,
    kwargs: KdtreeKwargs,
) -> PolarsResult<Series> {
    // Set up radius
    let radius = inputs[0].f64()?;
    // Set up params
    let dim = inputs[1..].len();
    let data = rechunk_to_frame(&inputs[1..])?;
    let nrows = data.height();
    let parallel = kwargs.parallel;
    let can_parallel = parallel && !context.parallel();
    let leaf_size = kwargs.leaf_size;
    let dist_func = which_distance(kwargs.metric.as_str(), dim)?;
    // Need to use C order because C order is row-contiguous
    let data = data.to_ndarray::<Float64Type>(IndexOrder::C)?;

    // Building the tree
    let binding = data.view();
    let tree = build_standard_kdtree(dim, leaf_size, &binding);

    if radius.len() == 1 {
        let r = radius.get(0).unwrap();
        let ca = query_nb_cnt(&tree, data.view(), &dist_func, r, can_parallel);
        Ok(ca.with_name("cnt").into_series())
    } else if radius.len() == nrows {
        let ca = if can_parallel {
            let nrows = data.shape()[0];
            let dim = data.shape()[1];
            let n_threads = POOL.current_num_threads();
            let splits = split_offsets(nrows, n_threads);
            let chunks_iter = splits.into_par_iter().map(|(offset, len)| {
                let piece = data.slice(s![offset..offset + len, 0..dim]);
                let rad = radius.slice(offset as i64, len);
                let out = piece
                    .axis_iter(Axis(0))
                    .zip(rad.into_iter())
                    .map(|(p, op_r)| {
                        let r = op_r?;
                        let sl = p.to_slice().unwrap();
                        tree.within_count(sl, r, &dist_func)
                            .map_or(None, |u| Some(u as u32))
                    });
                let ca = UInt32Chunked::from_iter_options("", out);
                ca.downcast_iter().cloned().collect::<Vec<_>>()
            });

            let chunks = POOL.install(|| chunks_iter.collect::<Vec<_>>());
            UInt32Chunked::from_chunk_iter("cnt", chunks.into_iter().flatten())
        } else {
            let mut builder: PrimitiveChunkedBuilder<UInt32Type> =
                PrimitiveChunkedBuilder::new("", nrows);
            radius
                .into_iter()
                .zip(data.axis_iter(Axis(0)))
                .for_each(|(rad, pt)| {
                    builder.append_option({
                        if let Some(r) = rad {
                            let s = pt.to_slice().unwrap(); // C order makes sure rows are contiguous
                            tree.within_count(s, r, &dist_func)
                                .map_or(None, |u| Some(u as u32))
                        } else {
                            None
                        }
                    })
                });
            builder.finish()
        };
        Ok(ca.with_name("cnt").into_series())
    } else {
        Err(PolarsError::ShapeMismatch(
            "Inputs must have the same length or one of them must be a scalar.".into(),
        ))
    }
}
