//! Lista encoders/decoders de vídeo por hardware em dois níveis:
//! - MFTs do Media Foundation (o que o caminho "MF puro" consegue usar);
//! - perfis de decode do D3D11VA (o que a GPU decodifica de fato, sem
//!   depender das extensões da Store).

use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface};

const CODECS: [(&str, GUID); 3] =
    [("H.264", MFVideoFormat_H264), ("HEVC", MFVideoFormat_HEVC), ("AV1", MFVideoFormat_AV1)];

pub fn run() -> anyhow::Result<()> {
    unsafe { windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED).ok()? };
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };

    println!("== Media Foundation: encoders de hardware ==");
    for (name, fmt) in CODECS {
        let out = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: fmt };
        list_mfts(name, MFT_CATEGORY_VIDEO_ENCODER, None, Some(&out), MFT_ENUM_FLAG_HARDWARE)?;
    }

    println!("\n== Media Foundation: decoders (hardware + software) ==");
    for (name, fmt) in CODECS {
        let inp = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: fmt };
        let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_ASYNCMFT.0);
        list_mfts(name, MFT_CATEGORY_VIDEO_DECODER, Some(&inp), None, flags)?;
    }

    println!("\n== D3D11VA: perfis de decode da GPU ==");
    d3d11_profiles()?;

    unsafe { MFShutdown()? };
    Ok(())
}

fn list_mfts(
    codec: &str,
    category: GUID,
    input: Option<&MFT_REGISTER_TYPE_INFO>,
    output: Option<&MFT_REGISTER_TYPE_INFO>,
    flags: MFT_ENUM_FLAG,
) -> anyhow::Result<()> {
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            category,
            MFT_ENUM_FLAG(flags.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
            input.map(|p| p as *const _),
            output.map(|p| p as *const _),
            &mut list,
            &mut count,
        )?
    };
    if list.is_null() || count == 0 {
        println!("  {codec:6} — nenhum");
        return Ok(());
    }
    let items = unsafe { std::slice::from_raw_parts_mut(list, count as usize) };
    for item in items.iter_mut() {
        let Some(act) = item.take() else { continue };
        let name = attr_string(&act, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "?".into());
        let hw = attr_string(&act, &MFT_ENUM_HARDWARE_URL_Attribute).is_some();
        println!("  {codec:6} — {name}{}", if hw { "  [HW]" } else { "" });
    }
    unsafe { CoTaskMemFree(Some(list as *const _)) };
    Ok(())
}

fn attr_string(act: &IMFActivate, key: &GUID) -> Option<String> {
    let mut ptr = windows::core::PWSTR::null();
    let mut len = 0u32;
    unsafe { act.GetAllocatedString(key, &mut ptr, &mut len).ok()? };
    let s = unsafe { ptr.to_string().ok() };
    unsafe { CoTaskMemFree(Some(ptr.0 as *const _)) };
    s
}

// GUIDs de perfis DXVA (dxva.h); não estão todos expostos pelo crate.
const PROFILES: [(&str, GUID); 6] = [
    ("H.264 (VLD, sem FGT)", GUID::from_u128(0x1b81be68_a0c7_11d3_b984_00c04f2e73c5)),
    ("HEVC Main", GUID::from_u128(0x5b11d51b_2f4c_4452_bcc3_09f2a1160cc0)),
    ("HEVC Main10", GUID::from_u128(0x107af0e0_ef1a_4d19_aba8_67a163073d13)),
    ("AV1 Profile0", GUID::from_u128(0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a)),
    ("VP9 Profile0", GUID::from_u128(0x463707f8_a1d0_4585_876d_83aa6d60b89e)),
    ("AV1 Profile1", GUID::from_u128(0x6936ff0f_45b1_4163_9cc1_646ef6946108)),
];

fn d3d11_profiles() -> anyhow::Result<()> {
    let mut device: Option<ID3D11Device> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?
    };
    let video: ID3D11VideoDevice = device.unwrap().cast()?;
    let n = unsafe { video.GetVideoDecoderProfileCount() };
    let mut supported = Vec::with_capacity(n as usize);
    for i in 0..n {
        supported.push(unsafe { video.GetVideoDecoderProfile(i)? });
    }
    for (name, guid) in PROFILES {
        let ok = supported.contains(&guid);
        let fmt = if name.contains("10") { DXGI_FORMAT_P010 } else { DXGI_FORMAT_NV12 };
        let fmt_ok = ok && unsafe { video.CheckVideoDecoderFormat(&guid, fmt) }.map(|b| b.as_bool()).unwrap_or(false);
        println!("  {name:22} {}", match (ok, fmt_ok) {
            (true, true) => "SIM",
            (true, false) => "perfil sim, formato de saída não",
            _ => "não",
        });
    }
    Ok(())
}
