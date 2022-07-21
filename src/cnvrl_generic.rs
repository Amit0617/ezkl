use halo2_proofs::{
    arithmetic::FieldExt,
    circuit::{AssignedCell, Layouter, Region, Value},
    plonk::{
        Advice, Assigned, Column, ConstraintSystem, Constraints, Error, Expression, Fixed, Selector,
    },
    poly::Rotation,
};
use std::marker::PhantomData;

mod image;
mod kernel;
mod util;

use image::*;
use kernel::*;
use util::*;

#[derive(Debug, Clone)]
struct Config<
    F: FieldExt,
    const KERNEL_HEIGHT: usize,
    const KERNEL_WIDTH: usize,
    const OUT_CHANNELS: usize,
    const STRIDE: usize,
    const IMAGE_HEIGHT: usize,
    const IMAGE_WIDTH: usize,
    const IN_CHANNELS: usize,
    const PADDING: usize,
> where
    [(); (IMAGE_HEIGHT + 2 * PADDING - KERNEL_HEIGHT) / STRIDE + 1]:,
    [(); (IMAGE_WIDTH + 2 * PADDING - KERNEL_WIDTH) / STRIDE + 1]:,
{
    selector: Selector,
    kernel: KernelConfig<F, KERNEL_HEIGHT, KERNEL_WIDTH>,
    image: ImageConfig<F, IMAGE_HEIGHT, IMAGE_WIDTH>,
    output: ImageConfig<
        F,
        { (IMAGE_HEIGHT + 2 * PADDING - KERNEL_HEIGHT) / STRIDE + 1 },
        { (IMAGE_WIDTH + 2 * PADDING - KERNEL_WIDTH) / STRIDE + 1 },
    >,
}

impl<
        F: FieldExt,
        const KERNEL_HEIGHT: usize,
        const KERNEL_WIDTH: usize,
        const OUT_CHANNELS: usize,
        const STRIDE: usize,
        const IMAGE_HEIGHT: usize,
        const IMAGE_WIDTH: usize,
        const IN_CHANNELS: usize,
        const PADDING: usize,
    >
    Config<
        F,
        KERNEL_HEIGHT,
        KERNEL_WIDTH,
        OUT_CHANNELS,
        STRIDE,
        IMAGE_HEIGHT,
        IMAGE_WIDTH,
        IN_CHANNELS,
        PADDING,
    >
where
    [(); (IMAGE_HEIGHT + 2 * PADDING - KERNEL_HEIGHT) / STRIDE + 1]:,
    [(); (IMAGE_WIDTH + 2 * PADDING - KERNEL_WIDTH) / STRIDE + 1]:,
    [(); IMAGE_HEIGHT * IMAGE_WIDTH]:,
    [(); ((IMAGE_HEIGHT + 2 * PADDING - KERNEL_HEIGHT) / STRIDE + 1)
        * ((IMAGE_WIDTH + 2 * PADDING - KERNEL_WIDTH) / STRIDE + 1)]:,
{
    fn configure(meta: &mut ConstraintSystem<F>, advices: Vec<Column<Advice>>) -> Self {
        let output_height = (IMAGE_HEIGHT + 2 * PADDING - KERNEL_HEIGHT) / STRIDE + 1;
        let output_width = (IMAGE_WIDTH + 2 * PADDING - KERNEL_WIDTH) / STRIDE + 1;

        let config = Self {
            selector: meta.selector(),
            kernel: KernelConfig::configure(meta),
            image: ImageConfig::configure(
                meta,
                advices[0..(IMAGE_HEIGHT * IMAGE_WIDTH)].try_into().unwrap(),
            ),
            output: ImageConfig::configure(
                meta,
                advices[0..(output_height * output_width)]
                    .try_into()
                    .unwrap(),
            ),
        };

        meta.create_gate("convolution", |meta| {
            let selector = meta.query_selector(config.selector);

            // Get output expressions for each input channel
            let intermediate_outputs = (0..IN_CHANNELS)
                .map(|rotation| {
                    let image = config.image.query(meta, Rotation(rotation as i32));
                    let kernel = config.kernel.query(meta, Rotation(rotation as i32));
                    convolution::<
                        _,
                        KERNEL_HEIGHT,
                        KERNEL_WIDTH,
                        IMAGE_HEIGHT,
                        IMAGE_WIDTH,
                        PADDING,
                        STRIDE,
                    >(kernel, image)
                })
                .collect();

            let witnessed_output = config.output.query(meta, Rotation(IN_CHANNELS as i32));
            let expected_output = op(intermediate_outputs, |a, b| a + b);

            let constraints = op_pair(witnessed_output, expected_output, |a, b| a - b)
                .flatten()
                .to_vec();

            Constraints::with_selector(selector, constraints)
        });

        config
    }

    fn assign_filter(
        &self,
        mut layouter: impl Layouter<F>,
        image: [Image<Value<F>, IMAGE_HEIGHT, IMAGE_WIDTH>; IN_CHANNELS],
        kernel: [Kernel<Value<F>, KERNEL_HEIGHT, KERNEL_WIDTH>; IN_CHANNELS],
    ) -> Result<
        Image<
            AssignedCell<Assigned<F>, F>,
            { (IMAGE_HEIGHT + 2 * PADDING - KERNEL_HEIGHT) / STRIDE + 1 },
            { (IMAGE_WIDTH + 2 * PADDING - KERNEL_WIDTH) / STRIDE + 1 },
        >,
        Error,
    > {
        layouter.assign_region(
            || "assign image and kernel",
            |mut region| {
                let mut offset = 0;
                self.selector.enable(&mut region, offset)?;

                let mut outputs = Vec::new();
                for (&image, &kernel) in image.iter().zip(kernel.iter()) {
                    let output = convolution::<
                        _,
                        KERNEL_HEIGHT,
                        KERNEL_WIDTH,
                        IMAGE_HEIGHT,
                        IMAGE_WIDTH,
                        PADDING,
                        STRIDE,
                    >(kernel, image);

                    self.image.assign_image_2d(&mut region, offset, image)?;
                    self.kernel.assign_kernel_2d(&mut region, offset, kernel)?;

                    offset += 1;
                    outputs.push(output);
                }

                let output = op(outputs, |a, b| a + b);
                self.output.assign_image_2d(&mut region, offset, output)
            },
        )
    }

    pub fn assign(
        &self,
        mut layouter: impl Layouter<F>,
        image: [Image<Value<F>, IMAGE_HEIGHT, IMAGE_WIDTH>; IN_CHANNELS],
        kernels: [[Kernel<Value<F>, KERNEL_HEIGHT, KERNEL_WIDTH>; IN_CHANNELS]; OUT_CHANNELS],
    ) -> Result<
        Vec<
            Image<
                AssignedCell<Assigned<F>, F>,
                { (IMAGE_HEIGHT + 2 * PADDING - KERNEL_HEIGHT) / STRIDE + 1 },
                { (IMAGE_WIDTH + 2 * PADDING - KERNEL_WIDTH) / STRIDE + 1 },
            >,
        >,
        Error,
    > {
        kernels
            .iter()
            .enumerate()
            .map(|(filter_idx, &kernel)| {
                self.assign_filter(
                    layouter.namespace(|| format!("filter: {:?}", filter_idx)),
                    image,
                    kernel,
                )
            })
            .collect::<Vec<_>>()
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::max;

    use super::*;
    use halo2_proofs::{
        arithmetic::{Field, FieldExt},
        circuit::SimpleFloorPlanner,
        dev::MockProver,
        plonk::Circuit,
    };
    use rand::rngs::OsRng;

    #[derive(Clone, Debug)]
    struct MyCircuit<
        F: FieldExt,
        const KERNEL_HEIGHT: usize,
        const KERNEL_WIDTH: usize,
        const OUT_CHANNELS: usize,
        const STRIDE: usize,
        const IMAGE_HEIGHT: usize,
        const IMAGE_WIDTH: usize,
        const IN_CHANNELS: usize,
        const PADDING: usize,
    > {
        image: [Image<Value<F>, IMAGE_HEIGHT, IMAGE_WIDTH>; IN_CHANNELS],
        kernels: [[Kernel<Value<F>, KERNEL_HEIGHT, KERNEL_WIDTH>; IN_CHANNELS]; OUT_CHANNELS],
    }

    impl<
            F: FieldExt,
            const KERNEL_HEIGHT: usize,
            const KERNEL_WIDTH: usize,
            const OUT_CHANNELS: usize,
            const STRIDE: usize,
            const IMAGE_HEIGHT: usize,
            const IMAGE_WIDTH: usize,
            const IN_CHANNELS: usize,
            const PADDING: usize,
        > Circuit<F>
        for MyCircuit<
            F,
            KERNEL_HEIGHT,
            KERNEL_WIDTH,
            OUT_CHANNELS,
            STRIDE,
            IMAGE_HEIGHT,
            IMAGE_WIDTH,
            IN_CHANNELS,
            PADDING,
        >
    where
        [(); (IMAGE_HEIGHT + 2 * PADDING - KERNEL_HEIGHT) / STRIDE + 1]:,
        [(); (IMAGE_WIDTH + 2 * PADDING - KERNEL_WIDTH) / STRIDE + 1]:,
        [(); IMAGE_HEIGHT * IMAGE_WIDTH]:,
        [(); ((IMAGE_HEIGHT + 2 * PADDING - KERNEL_HEIGHT) / STRIDE + 1)
            * ((IMAGE_WIDTH + 2 * PADDING - KERNEL_WIDTH) / STRIDE + 1)]:,
    {
        type Config = Config<
            F,
            KERNEL_HEIGHT,
            KERNEL_WIDTH,
            OUT_CHANNELS,
            STRIDE,
            IMAGE_HEIGHT,
            IMAGE_WIDTH,
            IN_CHANNELS,
            PADDING,
        >;
        //        Conv2d_then_Relu_Config<F, IH, IW, CHIN, CHOUT, KH, KW, OH, OW, BITS, LEN, INBITS, OUTBITS>;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            self.clone()
        }

        // Here we wire together the layers by using the output advice in each layer as input advice in the next (not with copying / equality).
        // This can be automated but we will sometimes want skip connections, etc. so we need the flexibility.
        fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
            let output_height = IMAGE_HEIGHT + 2 * PADDING - KERNEL_HEIGHT + 1;
            let output_width = IMAGE_WIDTH + 2 * PADDING - KERNEL_WIDTH + 1;

            let num_advices = max(output_height * output_width, IMAGE_HEIGHT * IMAGE_WIDTH);

            let advices = (0..num_advices)
                .map(|_| meta.advice_column())
                .collect::<Vec<_>>();

            Self::Config::configure(meta, advices)
        }

        fn synthesize(
            &self,
            config: Self::Config,
            layouter: impl Layouter<F>,
        ) -> Result<(), Error> {
            let output = config.assign(layouter, self.image, self.kernels)?;
            Ok(())
        }
    }

    #[test]
    fn test_cnvrl() {
        use halo2_proofs::pasta::pallas;

        const KERNEL_HEIGHT: usize = 3;
        const KERNEL_WIDTH: usize = 3;
        const OUT_CHANNELS: usize = 2;
        const STRIDE: usize = 2;
        const IMAGE_HEIGHT: usize = 7;
        const IMAGE_WIDTH: usize = 7;
        const IN_CHANNELS: usize = 2;
        const PADDING: usize = 2;

        let image = (0..IN_CHANNELS)
            .map(|_| matrix(|| Value::known(pallas::Base::random(OsRng))))
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let kernels = (0..OUT_CHANNELS)
            .map(|_| {
                (0..IN_CHANNELS)
                    .map(|_| matrix(|| Value::known(pallas::Base::random(OsRng))))
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap()
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();

        let circuit = MyCircuit::<
            pallas::Base,
            KERNEL_HEIGHT,
            KERNEL_WIDTH,
            OUT_CHANNELS,
            STRIDE,
            IMAGE_HEIGHT,
            IMAGE_WIDTH,
            IN_CHANNELS,
            PADDING,
        > {
            image,
            kernels,
        };

        let k = 4;
        let prover = MockProver::run(k, &circuit, vec![]).unwrap();
        prover.assert_satisfied();
    }
}
