use crate::layers::*;
use crate::layers::activation::*;

extern crate crossbeam;
extern crate num_cpus;
extern crate rayon;

use rayon::prelude::*;

/// Weight are arranged in flattened style:
/// every i^th consecutive (input size) items are the weight
/// of the input tensor to the i^th one in the output tensor,
/// e.g.:
///
///  - input: `[01,02;11,12]`
///  - output: `[21,22;31,32]`
///  - weight: `[21~01,21~02,21~11,21~12;22~01,22~02,22~11,22~12;...;...]`
///  - bias: `[21,22;31;32]`
///
/// When processed in parallel, each output coordinates a `mult`,
/// and each `chunk` includes many `mult`.
/// 
#[derive(Debug)]
pub struct Dense<T: NumT> {
    input_shape: Shape,
    output_shape: Shape,
    weight: Vec<T>,
    bias: Vec<T>,
    activation: Activation<T>,
}

impl<T: NumT> Dense<T> {
    pub fn new(i_shape: &Shape, o_shape: &Shape, act: Activation<T>) -> Self {
        let ilen = i_shape.size();
        let olen = o_shape.size();
        Dense::<T> {
            input_shape: i_shape.clone(),
            output_shape: o_shape.clone(),
            weight: vec![T::one(); ilen * olen],
            bias: vec![T::one(); olen],
            activation: act,
        }
    }
}

/// Helpers like `slice_iter(w, len, j)` is implemented to access the weight slice j,
/// containing len(== input length) elements
macro_rules! slice_iter {
    ($w: expr, $len: expr, $j: expr) => {
        $w[$j*$len..($j+1)*$len].into_iter()
    }
}
macro_rules! slice_iter_mut {
    ($w: expr, $len: expr, $j: expr) => {
        $w[$j*$len..($j+1)*$len].iter_mut()
    }
}

impl<T: NumT> Layer<T> for Dense<T> {
    fn forward_propagate(&self, input: &Tensor<T>, activate: bool) -> Result<Tensor<T>> {
        if input.shape != self.input_shape {
            return Err(ShapeMismatchError);
        }
        let mut output = Tensor::<T>::zeros(&self.output_shape);
        let olen = output.flattened.len();
        let ilen = input.flattened.len();

        let threads = num_cpus::get();
        let mults_per_chunk = olen / threads + 1;
        {
            let o_chunks = output.flattened.chunks_mut(mults_per_chunk);
            let w_chunks = self.weight.chunks(mults_per_chunk * ilen);
            crossbeam::scope(|spawner| {
                for (i, (o_chk, w_chk)) in o_chunks.zip(w_chunks).enumerate() {
                    spawner.spawn(move |_| {
                        for (j, o) in o_chk.into_iter().enumerate() {
                            *o = self.bias[i*mults_per_chunk + j];
                            // Do o = input.dot(w_chk[j])
                            for (k, &w) in slice_iter!(w_chk, ilen, j).enumerate() {
                                *o += w * input.flattened[k];
                            }
                            if activate {
                                *o = self.activation.call(*o);
                            }
                        }
                    });
                }
            }).unwrap(); 
        }
        Ok(output)
    }
    fn activate(&self, output: &Tensor<T>) -> Result<Tensor<T>> {
        if output.shape != self.output_shape {
            return Err(ShapeMismatchError);
        }
        let mut act_vec = vec![T::zero(); output.shape.size()];
        act_vec.par_iter_mut().zip(output.flattened.par_iter()).for_each(|(a, o)| {
            *a = self.activation.call(*o);
        });
        Ok(Tensor::<T>::new(&self.output_shape, act_vec).unwrap())
    }
    fn backpropagate_delta(&self, delta: &Tensor<T>, a_lst: &Tensor<T>, sigma_lst: &Activation<T>) -> Result<Tensor<T>> {
        if delta.shape != self.output_shape || a_lst.shape != self.input_shape {
            return Err(ShapeMismatchError);
        }
        let ilen = self.input_shape.size();
        let dlen = delta.flattened.len();

        // calculate products of weight and delta, to be sumed
        let mut prod = vec![T::zero(); self.weight.len()];
        let threads = num_cpus::get();
        let mults_per_chunk = dlen / threads + 1;
        {
            let d_chunks = delta.flattened.chunks(mults_per_chunk); // delta chunk
            let w_chunks = self.weight.chunks(mults_per_chunk * ilen); // weight chunk
            let p_chunks = prod.chunks_mut(mults_per_chunk * ilen); // prod chunk
            crossbeam::scope(|spawner| {
                for ((w_chk, p_chk), d_chk) in w_chunks.zip(p_chunks).zip(d_chunks) {
                    spawner.spawn(move |_| {
                        for (j, &d) in d_chk.into_iter().enumerate() {
                            // p[j] = w[j] * delta 
                            for (&w, p) in slice_iter!(w_chk, ilen, j).zip(slice_iter_mut!(p_chk, ilen, j)) {
                                *p = w * d;
                            }
                        }
                    });
                }
            }).unwrap(); 
        }

        // add those slices back
        let sum_prod = prod.par_chunks_mut(ilen).reduce_with(
            |s1, s2| {
                let len = s1.len();
                for i in 0..len {
                    let s = s1[i] + s2[i];
                    s1[i] = s; s2[i] = s;
                }
                s1
            }
        ).unwrap();

        let mut lst_delta = Tensor::<T>::zeros(&self.input_shape);
        lst_delta.flattened = sum_prod.to_vec();
        
        // dot product sigma-1(a^l) and w^Td^{l+1}
        lst_delta.flattened.par_iter_mut().zip(a_lst.flattened.par_iter()).for_each(|(d, a)| {
            *d *= sigma_lst.diff(*a);
        });

        Ok(lst_delta)
    }
    fn descend(&mut self, rate: T, delta: &Tensor<T>, a_lst: &Tensor<T>) -> Result<()> {
        if delta.shape != self.output_shape || a_lst.shape != self.input_shape {
            return Err(ShapeMismatchError);
        }
        // do weight update
        let dlen = delta.flattened.len();
        let alen = a_lst.flattened.len();

        let threads = num_cpus::get();
        let d_per_chunk = dlen / threads + 1;
        {
            let d_chunks = delta.flattened.chunks(d_per_chunk);
            let w_chunks = self.weight.chunks_mut(d_per_chunk * alen);
            crossbeam::scope(|spawner| {
                for (d_chk, w_chk) in d_chunks.zip(w_chunks) {
                    spawner.spawn(move |_| {
                        for (j, d) in d_chk.into_iter().enumerate() {
                            // Do w_chk[j] -= a * d[j]
                            for (k, w) in slice_iter_mut!(w_chk, alen, j).enumerate() {
                                *w -= rate * *d * a_lst.flattened[k];
                            }
                        }
                    });
                }
            }).unwrap(); 
        }

        // do bias update
        self.bias.par_iter_mut().zip(delta.flattened.par_iter()).for_each(|(b, d)| {
            *b -= rate * *d;
        });

        Ok(())
    }
}

#[test]
fn test_dense_forward() {
    let input = Tensor::<f64>::new(&Shape::new([2, 3]), vec![
        1., 7., 8.,
        -2., 3., 5.,
    ]).unwrap();
    let l = Dense::<f64> {
        input_shape: Shape::new([2, 3]),
        output_shape: Shape::new([2]),
        weight: vec![
            2., 1., -1., 3., 2., 1.,
            1., 0., 0., -2., 1., 0.,
        ],
        bias: vec![-5., -1.],
        activation: Activation::<f64>::No,
    };
    let output = Tensor::<f64>::new(&Shape::new([2]), vec![1., 7.]).unwrap();
    assert_eq!(l.forward_propagate(&input, true).unwrap(), output);
}

#[test]
fn test_dense_activate() {
    let l = Dense::<f64> {
        input_shape: Shape::new([2, 3]),
        output_shape: Shape::new([3, 4]),
        weight: vec![0.; 12_usize],
        bias: vec![0.; 12_usize],
        activation: Activation::<f64>::Sigmoid,
    };
    let output = Tensor::<f64>::new(&Shape::new([3, 4]), vec![
        -3., -2., -1., 0.,
        1., 2., 3., 4.,
        5., 6., 7., 8.,
    ]).unwrap();
    let mut ans_vec = vec![0.; 12];
    for (y, x) in ans_vec.iter_mut().zip(output.flattened.iter()) {
        *y = Activation::<f64>::Sigmoid.call(*x);
    }
    let answer = Tensor::<f64>::new(&Shape::new([3, 4]), ans_vec).unwrap();
    assert_eq!(l.activate(&output).unwrap(), answer);
}

#[test]
fn test_dense_backpropagate() {
    let lst_a = Tensor::<f64>::new(&Shape::new([2, 3]), vec![
        1., 7., 8.,
        -2., 3., 5.,
    ]).unwrap();
    let l = Dense::<f64> {
        input_shape: Shape::new([2, 3]),
        output_shape: Shape::new([2]),
        weight: vec![
            2., 1., -1., 3., 2., 1.,
            1., 0., 0., -2., 1., 0.,
        ],
        bias: vec![-5., -1.],
        activation: Activation::<f64>::No,
    };
    let delta = Tensor::<f64>::new(&Shape::new([2]), vec![1., 7.]).unwrap();
    let answer = Tensor::<f64>::new(&Shape::new([2, 3]), vec![
        9., 1., -1.,
        0., 9., 1.,
    ]).unwrap();
    assert_eq!(l.backpropagate_delta(&delta, &lst_a, &Activation::<f64>::Relu).unwrap(), answer);
}

#[test]
fn test_dense_descend() {
    let lst_a = Tensor::<f64>::new(&Shape::new([2, 3]), vec![
        1., 7., 8.,
        -2., 3., 5.,
    ]).unwrap();
    let mut l = Dense::<f64> {
        input_shape: Shape::new([2, 3]),
        output_shape: Shape::new([2]),
        weight: vec![
            2., 1., -1., 3., 2., 1.,
            1., 0., 0., -2., 1., 0.,
        ],
        bias: vec![-5., -1.],
        activation: Activation::<f64>::No,
    };
    let delta = Tensor::<f64>::new(&Shape::new([2]), vec![1., 7.]).unwrap();
    l.descend(0.1, &delta, &lst_a).unwrap();
    let w_ans = vec![
        2.-0.1, 1.-0.7, -1.-0.8, 3.+0.2, 2.-0.3, 1.-0.5,
        1.-0.7, 0.-4.9, 0.-5.6, -2.+1.4, 1.-2.1, 0.-3.5,
    ];
    let b_ans = vec![-5.-0.1, -1.-0.7];
    let eps = 1e-8;
    for (w, upd) in w_ans.into_iter().zip(l.weight.into_iter()) {
        assert!(
            (w - upd).abs() < eps,
            "expected {}, got {}", w, upd
        );
    }
    for (b, upd) in b_ans.into_iter().zip(l.bias.into_iter()) {
        assert!(
            (b - upd).abs() < eps,
            "expected {}, got {}", b, upd
        );
    }
}