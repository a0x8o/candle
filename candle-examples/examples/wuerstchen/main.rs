#![allow(unused)]

#[cfg(feature = "accelerate")]
extern crate accelerate_src;

#[cfg(feature = "mkl")]
extern crate intel_mkl_src;

use candle_transformers::models::stable_diffusion;
use candle_transformers::models::wuerstchen;

use anyhow::{Error as E, Result};
use candle::{DType, Device, IndexOp, Module, Tensor, D};
use clap::Parser;
use tokenizers::Tokenizer;

const GUIDANCE_SCALE: f64 = 7.5;
const RESOLUTION_MULTIPLE: f64 = 42.67;
const PRIOR_CIN: usize = 16;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The prompt to be used for image generation.
    #[arg(
        long,
        default_value = "A very realistic photo of a rusty robot walking on a sandy beach"
    )]
    prompt: String,

    #[arg(long, default_value = "")]
    uncond_prompt: String,

    /// Run on CPU rather than on GPU.
    #[arg(long)]
    cpu: bool,

    /// Enable tracing (generates a trace-timestamp.json file).
    #[arg(long)]
    tracing: bool,

    /// The height in pixels of the generated image.
    #[arg(long)]
    height: Option<usize>,

    /// The width in pixels of the generated image.
    #[arg(long)]
    width: Option<usize>,

    /// The decoder weight file, in .safetensors format.
    #[arg(long, value_name = "FILE")]
    decoder_weights: Option<String>,

    /// The CLIP weight file, in .safetensors format.
    #[arg(long, value_name = "FILE")]
    clip_weights: Option<String>,

    /// The CLIP weight file used by the prior model, in .safetensors format.
    #[arg(long, value_name = "FILE")]
    prior_clip_weights: Option<String>,

    /// The prior weight file, in .safetensors format.
    #[arg(long, value_name = "FILE")]
    prior_weights: Option<String>,

    /// The VQGAN weight file, in .safetensors format.
    #[arg(long, value_name = "FILE")]
    vqgan_weights: Option<String>,

    #[arg(long, value_name = "FILE")]
    /// The file specifying the tokenizer to used for tokenization.
    tokenizer: Option<String>,

    #[arg(long, value_name = "FILE")]
    /// The file specifying the tokenizer to used for prior tokenization.
    prior_tokenizer: Option<String>,

    /// The size of the sliced attention or 0 for automatic slicing (disabled by default)
    #[arg(long)]
    sliced_attention_size: Option<usize>,

    /// The number of steps to run the diffusion for.
    #[arg(long, default_value_t = 30)]
    n_steps: usize,

    /// The number of samples to generate.
    #[arg(long, default_value_t = 1)]
    num_samples: i64,

    /// The name of the final image to generate.
    #[arg(long, value_name = "FILE", default_value = "sd_final.png")]
    final_image: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelFile {
    Tokenizer,
    PriorTokenizer,
    Clip,
    PriorClip,
    Decoder,
    VqGan,
    Prior,
}

impl ModelFile {
    fn get(&self, filename: Option<String>) -> Result<std::path::PathBuf> {
        use hf_hub::api::sync::Api;
        match filename {
            Some(filename) => Ok(std::path::PathBuf::from(filename)),
            None => {
                let repo_main = "warp-ai/wuerstchen";
                let repo_prior = "warp-ai/wuerstchen-prior";
                let (repo, path) = match self {
                    Self::Tokenizer => (repo_main, "tokenizer/tokenizer.json"),
                    Self::PriorTokenizer => (repo_prior, "tokenizer/tokenizer.json"),
                    Self::Clip => (repo_main, "text_encoder/model.safetensors"),
                    Self::PriorClip => (repo_prior, "text_encoder/model.safetensors"),
                    Self::Decoder => (repo_main, "decoder/diffusion_pytorch_model.safetensors"),
                    Self::VqGan => (repo_main, "vqgan/diffusion_pytorch_model.safetensors"),
                    Self::Prior => (repo_prior, "prior/diffusion_pytorch_model.safetensors"),
                };
                let filename = Api::new()?.model(repo.to_string()).get(path)?;
                Ok(filename)
            }
        }
    }
}

fn output_filename(
    basename: &str,
    sample_idx: i64,
    num_samples: i64,
    timestep_idx: Option<usize>,
) -> String {
    let filename = if num_samples > 1 {
        match basename.rsplit_once('.') {
            None => format!("{basename}.{sample_idx}.png"),
            Some((filename_no_extension, extension)) => {
                format!("{filename_no_extension}.{sample_idx}.{extension}")
            }
        }
    } else {
        basename.to_string()
    };
    match timestep_idx {
        None => filename,
        Some(timestep_idx) => match filename.rsplit_once('.') {
            None => format!("{filename}-{timestep_idx}.png"),
            Some((filename_no_extension, extension)) => {
                format!("{filename_no_extension}-{timestep_idx}.{extension}")
            }
        },
    }
}

fn encode_prompt(
    prompt: &str,
    uncond_prompt: &str,
    tokenizer: std::path::PathBuf,
    clip_weights: std::path::PathBuf,
    clip_config: stable_diffusion::clip::Config,
    device: &Device,
) -> Result<Tensor> {
    let tokenizer = Tokenizer::from_file(tokenizer).map_err(E::msg)?;
    let pad_id = match &clip_config.pad_with {
        Some(padding) => *tokenizer.get_vocab(true).get(padding.as_str()).unwrap(),
        None => *tokenizer.get_vocab(true).get("<|endoftext|>").unwrap(),
    };
    println!("Running with prompt \"{prompt}\".");
    let mut tokens = tokenizer
        .encode(prompt, true)
        .map_err(E::msg)?
        .get_ids()
        .to_vec();
    let tokens_len = tokens.len();
    while tokens.len() < clip_config.max_position_embeddings {
        tokens.push(pad_id)
    }
    let tokens = Tensor::new(tokens.as_slice(), device)?.unsqueeze(0)?;

    let mut uncond_tokens = tokenizer
        .encode(uncond_prompt, true)
        .map_err(E::msg)?
        .get_ids()
        .to_vec();
    let uncond_tokens_len = uncond_tokens.len();
    while uncond_tokens.len() < clip_config.max_position_embeddings {
        uncond_tokens.push(pad_id)
    }
    let uncond_tokens = Tensor::new(uncond_tokens.as_slice(), device)?.unsqueeze(0)?;

    println!("Building the clip transformer.");
    let text_model =
        stable_diffusion::build_clip_transformer(&clip_config, clip_weights, device, DType::F32)?;
    let text_embeddings = text_model.forward_with_mask(&tokens, tokens_len)?;
    let uncond_embeddings = text_model.forward_with_mask(&uncond_tokens, uncond_tokens_len)?;
    let text_embeddings = Tensor::cat(&[uncond_embeddings, text_embeddings], 0)?;
    Ok(text_embeddings)
}

fn run(args: Args) -> Result<()> {
    use tracing_chrome::ChromeLayerBuilder;
    use tracing_subscriber::prelude::*;

    let Args {
        prompt,
        uncond_prompt,
        cpu,
        height,
        width,
        n_steps,
        tokenizer,
        final_image,
        sliced_attention_size,
        num_samples,
        clip_weights,
        prior_weights,
        vqgan_weights,
        decoder_weights,
        tracing,
        ..
    } = args;

    let _guard = if tracing {
        let (chrome_layer, guard) = ChromeLayerBuilder::new().build();
        tracing_subscriber::registry().with(chrome_layer).init();
        Some(guard)
    } else {
        None
    };

    let device = candle_examples::device(cpu)?;
    let height = height.unwrap_or(1024);
    let width = width.unwrap_or(1024);

    let prior_text_embeddings = {
        let tokenizer = ModelFile::PriorTokenizer.get(args.prior_tokenizer)?;
        let weights = ModelFile::PriorClip.get(args.prior_clip_weights)?;
        encode_prompt(
            &prompt,
            &uncond_prompt,
            tokenizer.clone(),
            weights,
            stable_diffusion::clip::Config::wuerstchen_prior(),
            &device,
        )?
    };
    println!("{prior_text_embeddings}");

    println!("Building the prior.");
    // https://huggingface.co/warp-ai/wuerstchen-prior/blob/main/prior/config.json
    let prior = {
        let prior_weights = ModelFile::Prior.get(prior_weights)?;
        let weights = unsafe { candle::safetensors::MmapedFile::new(prior_weights)? };
        let weights = weights.deserialize()?;
        let vb = candle_nn::VarBuilder::from_safetensors(vec![weights], DType::F32, &device);
        wuerstchen::prior::WPrior::new(
            /* c_in */ PRIOR_CIN, /* c */ 1536, /* c_cond */ 1280,
            /* c_r */ 64, /* depth */ 32, /* nhead */ 24, vb,
        )?
    };

    println!("Building the vqgan.");
    let _vqgan = {
        let vqgan_weights = ModelFile::VqGan.get(vqgan_weights)?;
        let weights = unsafe { candle::safetensors::MmapedFile::new(vqgan_weights)? };
        let weights = weights.deserialize()?;
        let vb = candle_nn::VarBuilder::from_safetensors(vec![weights], DType::F32, &device);
        wuerstchen::paella_vq::PaellaVQ::new(vb)?
    };

    println!("Building the decoder.");

    // https://huggingface.co/warp-ai/wuerstchen/blob/main/decoder/config.json
    let _decoder = {
        let decoder_weights = ModelFile::Decoder.get(decoder_weights)?;
        let weights = unsafe { candle::safetensors::MmapedFile::new(decoder_weights)? };
        let weights = weights.deserialize()?;
        let vb = candle_nn::VarBuilder::from_safetensors(vec![weights], DType::F32, &device);
        wuerstchen::diffnext::WDiffNeXt::new(
            /* c_in */ 4, /* c_out */ 4, /* c_r */ 64, /* c_cond */ 1024,
            /* clip_embd */ 1024, /* patch_size */ 2, vb,
        )?
    };

    let latent_height = (height as f64 / RESOLUTION_MULTIPLE).ceil() as usize;
    let latent_width = (width as f64 / RESOLUTION_MULTIPLE).ceil() as usize;
    let b_size = 1;
    for idx in 0..num_samples {
        let latents = Tensor::randn(
            0f32,
            1f32,
            (b_size, PRIOR_CIN, latent_height, latent_width),
            &device,
        )?;
        // TODO: latents denoising loop, use the scheduler values.
        let ratio = Tensor::ones(1, DType::F32, &device)?;
        let prior = prior.forward(&latents, &ratio, &prior_text_embeddings)?;

        let latents = ((latents * 42.)? - 1.)?;
        /*
        let timesteps = scheduler.timesteps();
        let latents = Tensor::randn(
            0f32,
            1f32,
            (bsize, 4, sd_config.height / 8, sd_config.width / 8),
            &device,
        )?;
        // scale the initial noise by the standard deviation required by the scheduler
        let mut latents = latents * scheduler.init_noise_sigma()?;

        println!("starting sampling");
        for (timestep_index, &timestep) in timesteps.iter().enumerate() {
            let start_time = std::time::Instant::now();
            let latent_model_input = Tensor::cat(&[&latents, &latents], 0)?;

            let latent_model_input = scheduler.scale_model_input(latent_model_input, timestep)?;
            let noise_pred =
                decoder.forward(&latent_model_input, timestep as f64, &text_embeddings)?;
            let noise_pred = noise_pred.chunk(2, 0)?;
            let (noise_pred_uncond, noise_pred_text) = (&noise_pred[0], &noise_pred[1]);
            let noise_pred =
                (noise_pred_uncond + ((noise_pred_text - noise_pred_uncond)? * GUIDANCE_SCALE)?)?;
            latents = scheduler.step(&noise_pred, timestep, &latents)?;
            let dt = start_time.elapsed().as_secs_f32();
            println!("step {}/{n_steps} done, {:.2}s", timestep_index + 1, dt);
        }
        */

        println!(
            "Generating the final image for sample {}/{}.",
            idx + 1,
            num_samples
        );
        /*
        let image = vae.decode(&(&latents / 0.18215)?)?;
        // TODO: Add the clamping between 0 and 1.
        let image = ((image / 2.)? + 0.5)?.to_device(&Device::Cpu)?;
        let image = (image * 255.)?.to_dtype(DType::U8)?.i(0)?;
        let image_filename = output_filename(&final_image, idx + 1, num_samples, None);
        candle_examples::save_image(&image, image_filename)?
        */
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    run(args)
}