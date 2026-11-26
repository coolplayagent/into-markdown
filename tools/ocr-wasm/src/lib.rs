//! Single-use, import-free WebAssembly tensor worker for the two official PP-OCR graphs.
//! The host bounds linear memory and execution time. All offsets address this instance only.

use rten::{ModelOptions, op_registry};
use rten_tensor::{AsView, Layout, NdTensor, Tensor};
use std::cell::RefCell;

#[derive(Default)]
struct Buffers {
    model: Vec<u8>,
    input: Vec<f32>,
    output: Option<Tensor<f32>>,
}

thread_local! {
    static BUFFERS: RefCell<Buffers> = RefCell::default();
}

#[unsafe(no_mangle)]
pub extern "C" fn model_buffer(bytes: usize) -> usize {
    BUFFERS.with_borrow_mut(|state| {
        state.model.resize(bytes, 0);
        state.model.as_mut_ptr() as usize
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn input_buffer(elements: usize) -> usize {
    BUFFERS.with_borrow_mut(|state| {
        state.input.resize(elements, 0.0);
        state.input.as_mut_ptr() as usize
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn infer(batch: usize, channels: usize, height: usize, width: usize) -> usize {
    BUFFERS.with_borrow_mut(|state| {
        let ops = op_registry!(
            Add,
            AveragePool,
            BatchNormalization,
            Concat,
            Conv,
            ConvTranspose,
            Div,
            Erf,
            GlobalAveragePool,
            HardSigmoid,
            Identity,
            MatMul,
            MaxPool,
            Mul,
            ReduceMean,
            Relu,
            Resize,
            Sigmoid,
            Softmax,
            Squeeze,
            Transpose,
            Unsqueeze
        );
        let Ok(model) = ModelOptions::with_ops(ops).load(std::mem::take(&mut state.model)) else {
            return 0;
        };
        let input =
            NdTensor::from_data([batch, channels, height, width], std::mem::take(&mut state.input));
        let Ok(value) = model.run_one(input.view().into(), None) else {
            return 0;
        };
        let Ok(output) = Tensor::<f32>::try_from(value) else {
            return 0;
        };
        let rank = output.ndim();
        state.output = Some(output.into_contiguous().into());
        rank
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn output_dimension(axis: usize) -> usize {
    BUFFERS.with_borrow(|state| state.output.as_ref().unwrap().size(axis))
}

#[unsafe(no_mangle)]
pub extern "C" fn output_buffer() -> usize {
    BUFFERS.with_borrow(|state| state.output.as_ref().unwrap().data().unwrap().as_ptr() as usize)
}
