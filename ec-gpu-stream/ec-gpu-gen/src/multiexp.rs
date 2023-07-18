use std::ops::AddAssign;
use std::sync::{Arc, RwLock};
use std::ptr::NonNull;

use blstrs::{G1Affine,G1Projective,G2Affine,G2Projective, Bls12,Scalar};
// extern crate rustacuda;
use rustacuda::prelude::*;
use rustacuda::launch;
use rustacuda::memory::DeviceBuffer;
use rustacuda::memory::AsyncCopyDestination;
use std::ffi::CString;
use std::time::Instant;
// use rustacuda::error::CudaError;
// use rustacuda::error::CudaResult;

use ec_gpu::GpuName;
use ff::PrimeField;
use group::{prime::PrimeCurveAffine, Group};
use log::{error, info};
use rust_gpu_tools::{program_closures, Device, Program};
use yastl::Scope;
use crate::{
    error::{EcError, EcResult},
    threadpool::Worker,
};
// use std::error::Error;
/// On the GPU, the exponents are split into windows, this is the maximum number of such windows.
const MAX_WINDOW_SIZE: usize = 10;
/// In CUDA this is the number of blocks per grid (grid size).
const LOCAL_WORK_SIZE: usize = 128;
/// Let 20% of GPU memory be free, this is an arbitrary value.
const MEMORY_PADDING: f64 = 0.2f64;
/// The Nvidia Ampere architecture is compute capability major version 8.
const AMPERE: u32 = 8;

/// Divide and ceil to the next value.
const fn div_ceil(a: usize, b: usize) -> usize {
    if a % b == 0 {
        a / b
    } else {
        (a / b) + 1
    }
}

/// The number of units the work is split into. One unit will result in one CUDA thread.
///
/// Based on empirical results, it turns out that on Nvidia devices with the Ampere architecture,
/// it's faster to use two times the number of work units.
const fn work_units(compute_units: u32, compute_capabilities: Option<(u32, u32)>) -> usize {
    match compute_capabilities {
        Some((AMPERE, _)) => LOCAL_WORK_SIZE * compute_units as usize * 2,
        _ => LOCAL_WORK_SIZE * compute_units as usize,
    }
}

/// Multiexp kernel for a single GPU.
pub struct SingleMultiexpKernel<'a, G>
where
    G: PrimeCurveAffine,
{
    program: Program,
    /// The number of exponentiations the GPU can handle in a single execution of the kernel.
    n: usize,
    /// The number of units the work is split into. It will results in this amount of threads on
    /// the GPU.
    work_units: usize,
    /// An optional function which will be called at places where it is possible to abort the
    /// multiexp calculations. If it returns true, the calculation will be aborted with an
    /// [`EcError::Aborted`].
    maybe_abort: Option<&'a (dyn Fn() -> bool + Send + Sync)>,

    _phantom: std::marker::PhantomData<G::Scalar>,
}

/// Calculates the maximum number of terms that can be put onto the GPU memory.
fn calc_chunk_size<G>(mem: u64, work_units: usize) -> usize
where
    G: PrimeCurveAffine,
    G::Scalar: PrimeField,
{
    let aff_size = std::mem::size_of::<G>();
    let exp_size = exp_size::<G::Scalar>();
    let proj_size = std::mem::size_of::<G::Curve>();

    // Leave `MEMORY_PADDING` percent of the memory free.
    let max_memory = ((mem as f64) * (1f64 - MEMORY_PADDING)) as usize;
    // The amount of memory (in bytes) of a single term.
    let term_size = aff_size + exp_size;
    // The number of buckets needed for one work unit
    let max_buckets_per_work_unit = 1 << MAX_WINDOW_SIZE;
    // The amount of memory (in bytes) we need for the intermediate steps (buckets).
    let buckets_size = work_units * max_buckets_per_work_unit * proj_size;
    // The amount of memory (in bytes) we need for the results.
    let results_size = work_units * proj_size;

    (max_memory - buckets_size - results_size) / term_size
}

/// The size of the exponent in bytes.
///
/// It's the actual bytes size it needs in memory, not it's theoratical bit size.
fn exp_size<F: PrimeField>() -> usize {
    std::mem::size_of::<F::Repr>()
}

impl<'a, G> SingleMultiexpKernel<'a, G>
where
    G: PrimeCurveAffine + GpuName,
{
    /// Create a new Multiexp kernel instance for a device.
    ///
    /// The `maybe_abort` function is called when it is possible to abort the computation, without
    /// leaving the GPU in a weird state. If that function returns `true`, execution is aborted.
    pub fn create(
        program: Program,
        device: &Device,
        maybe_abort: Option<&'a (dyn Fn() -> bool + Send + Sync)>,
    ) -> EcResult<Self> {
        let mem = device.memory();
        let compute_units = device.compute_units();
        let compute_capability = device.compute_capability();
        let work_units = work_units(compute_units, compute_capability);
        let chunk_size = calc_chunk_size::<G>(mem, work_units);

        Ok(SingleMultiexpKernel {
            program,
            n: chunk_size,
            work_units,
            maybe_abort,
            _phantom: std::marker::PhantomData,
        })
    }



pub fn create_buffer_from_slice<T>(&self, slice: &[T]){
    let length = slice.len();
    let bytes_len = length * std::mem::size_of::<T>();
    let bytes = unsafe {
    std::slice::from_raw_parts(slice.as_ptr() as *const T as *const u8, bytes_len)
     };

     for i in 0..48{
        println!("{:?} ",bytes[i]);
     }
     println!("\n");
     println!("\n");
    // println!("******bytes_len: {:?}\n",bytes_len);
     //println!("******exps:{:?} {:?}",bytes[0],bytes[1]);   
}









    /// Run the actual multiexp computation on the GPU.
    ///
    /// The number of `bases` and `exponents` are determined by [`SingleMultiexpKernel`]`::n`, this
    /// means that it is guaranteed that this amount of calculations fit on the GPU this kernel is
    /// running on.
    pub fn multiexp(
        &self,
        bases: &[G],
        exponents: &[<G::Scalar as PrimeField>::Repr],
    ) -> EcResult<G::Curve> {
        assert_eq!(bases.len(), exponents.len());

        if let Some(maybe_abort) = &self.maybe_abort {
            if maybe_abort() {
                return Err(EcError::Aborted);
            }
        }
        let window_size = self.calc_window_size(bases.len());
        // windows_size * num_windows needs to be >= 256 in order for the kernel to work correctly.
        let num_windows = div_ceil(256, window_size);
        let num_groups = self.work_units / num_windows;
        let bucket_len = 1 << window_size;

        // Each group will have `num_windows` threads and as there are `num_groups` groups, there will
        // be `num_groups` * `num_windows` threads in total.
        // Each thread will use `num_groups` * `num_windows` * `bucket_len` buckets.

        let closures = program_closures!(|program, _arg| -> EcResult<Vec<G::Curve>> {


let s = Instant::now();
            let base_buffer = program.create_buffer_from_slice(bases)?;
            // self.create_buffer_from_slice(bases);
            let exp_buffer = program.create_buffer_from_slice(exponents)?;
            

            // It is safe as the GPU will initialize that buffer
            let bucket_buffer =
                unsafe { program.create_buffer::<G::Curve>(self.work_units * bucket_len)? };
            // It is safe as the GPU will initialize that buffer
            let result_buffer = unsafe { program.create_buffer::<G::Curve>(self.work_units)? };

            // The global work size follows CUDA's definition and is the number of
            // `LOCAL_WORK_SIZE` sized thread groups.
            let global_work_size = div_ceil(num_windows * num_groups, LOCAL_WORK_SIZE);

            let kernel_name = format!("{}_multiexp", G::name());
            let kernel = program.create_kernel(&kernel_name, global_work_size, LOCAL_WORK_SIZE)?;

            kernel
                .arg(&base_buffer)
                .arg(&bucket_buffer)
                .arg(&result_buffer)
                .arg(&exp_buffer)
                .arg(&(bases.len() as u32))
                .arg(&(num_groups as u32))
                .arg(&(num_windows as u32))
                .arg(&(window_size as u32))
                .run()?;

            let mut results = vec![G::Curve::identity(); self.work_units];
            program.read_into_buffer(&result_buffer, &mut results)?;
println!("************************************************************************bellperson: {:?}\n",s.elapsed());
            Ok(results)
        });

        let results = self.program.run(closures, ())?;
//////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

let my_window_size = window_size;
let my_num_windows = num_windows;
let my_num_groups = num_groups;
let my_bucket_len = bucket_len / 2;
println!("************************my_window_size:{:?}\n",my_window_size);
    // let my_bucket_len = 1024 as usize;
    // let my_window_size = 11 as usize;
    // let my_num_windows = 24 as usize;
    // let my_num_groups = 298 as usize;

let kernel_name = format!("{}_multiexp", G::name());
println!("***********:{:?}\n",kernel_name);
let str1 = "blstrs__g1__G1Affine_multiexp";
let str2 = "blstrs__g2__G2Affine_multiexp";
let mut my_results = vec![G1Projective::identity(); my_num_groups * my_num_windows];
let mut my_results_2=vec![G2Projective::identity(); my_num_groups * my_num_windows];

let n = bases.len() as usize;
let my_global_work_size =(my_num_windows * my_num_groups + LOCAL_WORK_SIZE - 1) / LOCAL_WORK_SIZE;
let row_nums = div_ceil(n,my_global_work_size * LOCAL_WORK_SIZE);

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////
if kernel_name == str1{

rustacuda::init(CudaFlags::empty());
let device_my = rustacuda::device::Device::get_device(0).unwrap();
let _ctx = Context::create_and_push(ContextFlags::MAP_HOST | ContextFlags::SCHED_AUTO, device_my);

let image = include_bytes!("/hdd/wangfeng/Cuda_G1_New/CudaDevice/msm_all_2_4.fatbin");
let module = Module::load_from_bytes(image).unwrap();
let stream_1 = Stream::new(StreamFlags::NON_BLOCKING, None).unwrap();
let stream_2 = Stream::new(StreamFlags::NON_BLOCKING, None).unwrap();
let stream_3 = Stream::new(StreamFlags::NON_BLOCKING, None).unwrap();
// my_results = vec![G1Projective::identity(); my_num_groups * my_num_windows];
unsafe{

    

let s1 = Instant::now();
    let e_bytes = std::slice::from_raw_parts(exponents.as_ptr() as *const G::Scalar as *const Scalar, bases.len());
    let s_bytes = std::slice::from_raw_parts(bases.as_ptr() as *const G::Curve as *const G1Affine, bases.len());

println!("*****************************************切片:{:?}\n",s1.elapsed());

let s2 = Instant::now();
    let mut exps_trans_buffer = <DeviceBuffer<i32>>::uninitialized(n * my_num_windows).unwrap();
    let mut my_bucket_buffer = <DeviceBuffer<G1Projective>>::uninitialized(my_num_groups * my_num_windows * my_bucket_len).unwrap();
    let mut my_result_buffer = <DeviceBuffer<G1Projective>>::uninitialized(my_num_groups * my_num_windows).unwrap();
println!("*****************************************uninitialized:{:?}\n",s2.elapsed());

let s3 = Instant::now();
    let function_name = CString::new("Exps_Handle_new").unwrap();
    let function_name_2 = CString::new("blstrs__g1__G1Affine_multiexp").unwrap();
    let Exps_Handle_new = module.get_function(&function_name).unwrap();
    let msm = module.get_function(&function_name_2).unwrap();
println!("****************************************加载kernel:{:?}\n",s3.elapsed());


let st = Instant::now();   
    let mut my_exp_buffer = DeviceBuffer::from_slice_async(&e_bytes,&stream_1).unwrap();
    let re_1 = launch!(Exps_Handle_new<<<my_global_work_size as u32,LOCAL_WORK_SIZE as u32,0,stream_1>>>(my_exp_buffer.as_device_ptr(),
    exps_trans_buffer.as_device_ptr(),n,my_window_size,my_num_windows,row_nums)).unwrap();

    let mut  my_base_buffer = DeviceBuffer::from_slice_async(&s_bytes,&stream_2).unwrap();
    let re_2 = launch!(msm<<<my_global_work_size as u32,LOCAL_WORK_SIZE as u32,0,stream_2>>>(
        my_base_buffer.as_device_ptr(),my_bucket_buffer.as_device_ptr(),my_result_buffer.as_device_ptr(),exps_trans_buffer.as_device_ptr(),
        n,my_num_groups,my_num_windows,my_window_size)).unwrap();
    my_result_buffer.async_copy_to(&mut my_results,&stream_2);

    stream_1.synchronize().unwrap();
    stream_2.synchronize().unwrap();
    drop(my_exp_buffer);
    drop(exps_trans_buffer);
    drop(my_bucket_buffer);
    drop(my_result_buffer);
    drop(my_base_buffer);
println!("************************************************************************my: {:?}\n",st.elapsed());

};
}
///////////////////////////////////////////////////////////////////////////////////////////////////////////
else{
    rustacuda::init(CudaFlags::empty());
    let device_my = rustacuda::device::Device::get_device(0).unwrap();
    let _ctx = Context::create_and_push(ContextFlags::MAP_HOST | ContextFlags::SCHED_AUTO, device_my);
    
    let image = include_bytes!("/hdd/wangfeng/Cuda_G1_New/CudaDevice/msm_all_2_4_2.fatbin");
    let module = Module::load_from_bytes(image).unwrap();
    let stream_1 = Stream::new(StreamFlags::NON_BLOCKING, None).unwrap();
    let stream_2 = Stream::new(StreamFlags::NON_BLOCKING, None).unwrap();
    let stream_3 = Stream::new(StreamFlags::NON_BLOCKING, None).unwrap();
    unsafe{

let s1 = Instant::now();
        let e_bytes = std::slice::from_raw_parts(exponents.as_ptr() as *const G::Scalar as *const Scalar, bases.len());
        let s_bytes = std::slice::from_raw_parts(bases.as_ptr() as *const G::Curve as *const G2Affine, bases.len());
println!("*****************************************切片:{:?}\n",s1.elapsed());

let s2 = Instant::now();
    let mut exps_trans_buffer = <DeviceBuffer<i32>>::uninitialized(n * my_num_windows).unwrap();
    let mut my_bucket_buffer = <DeviceBuffer<G2Projective>>::uninitialized(my_num_groups * my_num_windows * my_bucket_len).unwrap();
    let mut my_result_buffer = <DeviceBuffer<G2Projective>>::uninitialized(my_num_groups * my_num_windows).unwrap();
println!("*****************************************uninitialized:{:?}\n",s2.elapsed());

let s3 = Instant::now();
    let function_name = CString::new("Exps_Handle_new").unwrap();
    let function_name_2 = CString::new("blstrs__g2__G2Affine_multiexp").unwrap();
    let Exps_Handle_new = module.get_function(&function_name).unwrap();
    let msm = module.get_function(&function_name_2).unwrap();
println!("****************************************加载kernel:{:?}\n",s3.elapsed());

let st = Instant::now();   
    let mut my_exp_buffer = DeviceBuffer::from_slice_async(&e_bytes,&stream_1).unwrap();
    let re_1 = launch!(Exps_Handle_new<<<my_global_work_size as u32,LOCAL_WORK_SIZE as u32,0,stream_1>>>(my_exp_buffer.as_device_ptr(),
    exps_trans_buffer.as_device_ptr(),n,my_window_size,my_num_windows,row_nums)).unwrap();

    let mut  my_base_buffer = DeviceBuffer::from_slice_async(&s_bytes,&stream_2).unwrap();
    let re_2 = launch!(msm<<<my_global_work_size as u32,LOCAL_WORK_SIZE as u32,0,stream_2>>>(
        my_base_buffer.as_device_ptr(),my_bucket_buffer.as_device_ptr(),my_result_buffer.as_device_ptr(),exps_trans_buffer.as_device_ptr(),
        n,my_num_groups,my_num_windows,my_window_size)).unwrap();
    my_result_buffer.async_copy_to(&mut my_results_2,&stream_2);

    stream_1.synchronize().unwrap();
    stream_2.synchronize().unwrap();
    drop(my_exp_buffer);
    drop(exps_trans_buffer);
    drop(my_bucket_buffer);
    drop(my_result_buffer);
    drop(my_base_buffer);

println!("************************************************************************my: {:?}\n",st.elapsed());

    };
}



////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////
        // Using the algorithm below, we can calculate the final result by accumulating the results
        // of those `NUM_GROUPS` * `NUM_WINDOWS` threads.
        let mut y;
if kernel_name == str1{
    let mut acc = G1Projective::identity();
    for i in 0..my_num_windows {
        let w = my_window_size;
        for _ in 0..w {
            acc = acc.double();
        }
        for g in 0..my_num_groups {
            acc.add_assign(&my_results[g * my_num_windows + i]);
        }
    }
    let mut x = [0 as u8 ; 144];
    unsafe {
    let mut acc_type =   std::slice::from_raw_parts(&acc as *const G1Projective as *const u8, 144);
    x[..].copy_from_slice(acc_type);
    let ptrtt = NonNull::new(&mut x as *mut _).expect("null pointer");
    let casted_ptrtt = ptrtt.cast::<G::Curve>();
    let raw_ptrtt: *mut G::Curve = casted_ptrtt.as_ptr();
    y = raw_ptrtt.read_volatile();
    };  
}
else{
    let mut acc = G2Projective::identity();
    for i in 0..my_num_windows {
        let w = my_window_size;
        for _ in 0..w {
            acc = acc.double();
        }
        for g in 0..my_num_groups {
            acc.add_assign(&my_results_2[g * my_num_windows + i]);
        }
    }
    let mut x = [0 as u8 ; 288];
    unsafe {
    let mut acc_type =   std::slice::from_raw_parts(&acc as *const G2Projective as *const u8, 288);
    x[..].copy_from_slice(acc_type);
    let ptrtt = NonNull::new(&mut x as *mut _).expect("null pointer");
    let casted_ptrtt = ptrtt.cast::<G::Curve>();
    let raw_ptrtt: *mut G::Curve = casted_ptrtt.as_ptr();
    y = raw_ptrtt.read_volatile();
    };
}
        Ok(y)
    }

    /// Calculates the window size, based on the given number of terms.
    ///
    /// For best performance, the window size is reduced, so that maximum parallelism is possible.
    /// If you e.g. have put only a subset of the terms into the GPU memory, then a smaller window
    /// size leads to more windows, hence more units to work on, as we split the work into
    /// `num_windows * num_groups`.
    fn calc_window_size(&self, num_terms: usize) -> usize {
        // The window size was determined by running the `gpu_multiexp_consistency` test and
        // looking at the resulting numbers.
        let window_size = ((div_ceil(num_terms, self.work_units) as f64).log2() as usize) + 2;
        std::cmp::min(window_size, MAX_WINDOW_SIZE)
    }
}

/// A struct that containts several multiexp kernels for different devices.
pub struct MultiexpKernel<'a, G>
where
    G: PrimeCurveAffine,
{
    kernels: Vec<SingleMultiexpKernel<'a, G>>,
}

impl<'a, G> MultiexpKernel<'a, G>
where
    G: PrimeCurveAffine + GpuName,
{
    /// Create new kernels, one for each given device.
    pub fn create(programs: Vec<Program>, devices: &[&Device]) -> EcResult<Self> {
        Self::create_optional_abort(programs, devices, None)
    }

    /// Create new kernels, one for each given device, with early abort hook.
    ///
    /// The `maybe_abort` function is called when it is possible to abort the computation, without
    /// leaving the GPU in a weird state. If that function returns `true`, execution is aborted.
    pub fn create_with_abort(
        programs: Vec<Program>,
        devices: &[&Device],
        maybe_abort: &'a (dyn Fn() -> bool + Send + Sync),
    ) -> EcResult<Self> {
        Self::create_optional_abort(programs, devices, Some(maybe_abort))
    }

    fn create_optional_abort(
        programs: Vec<Program>,
        devices: &[&Device],
        maybe_abort: Option<&'a (dyn Fn() -> bool + Send + Sync)>,
    ) -> EcResult<Self> {
        let kernels: Vec<_> = programs
            .into_iter()
            .zip(devices.iter())
            .filter_map(|(program, device)| {
                let device_name = program.device_name().to_string();
                let kernel = SingleMultiexpKernel::create(program, device, maybe_abort);
                if let Err(ref e) = kernel {
                    error!(
                        "Cannot initialize kernel for device '{}'! Error: {}",
                        device_name, e
                    );
                }
                kernel.ok()
            })
            .collect();

        if kernels.is_empty() {
            return Err(EcError::Simple("No working GPUs found!"));
        }
        info!("Multiexp: {} working device(s) selected.", kernels.len());
        for (i, k) in kernels.iter().enumerate() {
            info!(
                "Multiexp: Device {}: {} (Chunk-size: {})",
                i,
                k.program.device_name(),
                k.n
            );
        }
        Ok(MultiexpKernel { kernels })
    }

    /// Calculate multiexp on all available GPUs.
    ///
    /// It needs to run within a [`yastl::Scope`]. This method usually isn't called directly, use
    /// [`MultiexpKernel::multiexp`] instead.
    pub fn parallel_multiexp<'s>(
        &'s mut self,
        scope: &Scope<'s>,
        bases: &'s [G],
        exps: &'s [<G::Scalar as PrimeField>::Repr],
        results: &'s mut [G::Curve],
        error: Arc<RwLock<EcResult<()>>>,
    ) {
        let num_devices = self.kernels.len();
        let num_exps = exps.len();
        // The maximum number of exponentiations per device.
        let chunk_size = ((num_exps as f64) / (num_devices as f64)).ceil() as usize;

        for (((bases, exps), kern), result) in bases
            .chunks(chunk_size)
            .zip(exps.chunks(chunk_size))
            // NOTE vmx 2021-11-17: This doesn't need to be a mutable iterator. But when it isn't
            // there will be errors that the OpenCL CommandQueue cannot be shared between threads
            // safely.
            .zip(self.kernels.iter_mut())
            .zip(results.iter_mut())
        {
            let error = error.clone();
            scope.execute(move || {
                let mut acc = G::Curve::identity();
                for (bases, exps) in bases.chunks(kern.n).zip(exps.chunks(kern.n)) {
                    if error.read().unwrap().is_err() {
                        break;
                    }
                    match kern.multiexp(bases, exps) {
                        Ok(result) => acc.add_assign(&result),
                        Err(e) => {
                            *error.write().unwrap() = Err(e);
                            break;
                        }
                    }
                }
                if error.read().unwrap().is_ok() {
                    *result = acc;
                }
            });
        }
    }

    /// Calculate multiexp.
    ///
    /// This is the main entry point.
    pub fn multiexp(
        &mut self,
        pool: &Worker,
        bases_arc: Arc<Vec<G>>,
        exps: Arc<Vec<<G::Scalar as PrimeField>::Repr>>,
        skip: usize,
    ) -> EcResult<G::Curve> {
        // Bases are skipped by `self.1` elements, when converted from (Arc<Vec<G>>, usize) to Source
        // https://github.com/zkcrypto/bellman/blob/10c5010fd9c2ca69442dc9775ea271e286e776d8/src/multiexp.rs#L38
        let bases = &bases_arc[skip..(skip + exps.len())];
        let exps = &exps[..];
        let mut results = Vec::new();
        let error = Arc::new(RwLock::new(Ok(())));

        pool.scoped(|s| {
            results = vec![G::Curve::identity(); self.kernels.len()];
            self.parallel_multiexp(s, bases, exps, &mut results, error.clone());
        });

        Arc::try_unwrap(error)
            .expect("only one ref left")
            .into_inner()
            .unwrap()?;

        let mut acc = G::Curve::identity();
        for r in results {
            acc.add_assign(&r);
        }

        Ok(acc)
    }

    /// Returns the number of kernels (one per device).
    pub fn num_kernels(&self) -> usize {
        self.kernels.len()
    }
}