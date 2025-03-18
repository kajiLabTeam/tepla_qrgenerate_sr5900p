use crate::analyzer::analyze_tcp_data;
use crate::display::TapeDisplay;
use crate::protocol::notify_data_stream;
use crate::protocol::StartPrintRequest;
use crate::protocol::StatusRequest;
use crate::protocol::StopPrintRequest;
use crate::PrinterStatus;
use crate::Tape;
use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use argh::FromArgs;
//use barcoders::sym::code39::Code39;
use embedded_graphics::geometry::Dimensions;
use embedded_graphics::geometry::Point;
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::Size;
use embedded_graphics::primitives::PrimitiveStyle;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::primitives::StyledDrawable;
use embedded_graphics::text::Alignment;
use embedded_graphics::text::Baseline;
use embedded_graphics::text::Text;
use embedded_graphics::text::TextStyleBuilder;
use embedded_graphics::Drawable;
use image::Luma;
use qrcode::QrCode;
//use regex::Regex;
//use std::fs;
use std::fs::File;
use std::io::prelude::Write;
use std::io::BufWriter;
use std::net::TcpStream;
use std::net::UdpSocket;
use std::num::Wrapping;
use std::path::Path;
use std::thread;
use std::time;

pub fn mm_to_px(mm: f32) -> i32 {
    const DPI: f32 = 360.0;
    const MM_TO_INCH: f32 = 10.0 / 254.0;
    (mm * DPI * MM_TO_INCH).floor() as i32
}

fn print_tcp_data(device_ip: &str, data: &[u8]) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").context("failed to bind")?;
    let info = StatusRequest::send(&socket, device_ip)?;
    println!("{:?}", info);
    if let PrinterStatus::SomeTape(t) = info {
        println!("Tape is {:?}, start printing...", t);
    } else {
        println!("Unexpected state. Aborting...");
        std::process::exit(1);
    }
    StartPrintRequest::send(&socket, device_ip)?;
    thread::sleep(time::Duration::from_millis(500));
    let mut stream = TcpStream::connect(device_ip.to_string() + ":9100")?;
    thread::sleep(time::Duration::from_millis(500));
    notify_data_stream(&socket, device_ip)?;
    thread::sleep(time::Duration::from_millis(500));
    stream.write_all(data)?;

    println!("Print data is sent. Waiting...");
    loop {
        thread::sleep(time::Duration::from_millis(500));
        let info = StatusRequest::send(&socket, device_ip)?;
        println!("{:?}", info);
        if let PrinterStatus::Printing = info {
            continue;
        }
        break;
    }

    StopPrintRequest::send(&socket, device_ip)?;

    Ok(())
}

fn gen_tcp_data(td: &TapeDisplay) -> Result<Vec<u8>> {
    let mut tcp_data: Vec<u8> = Vec::new();
    tcp_data.append(&mut vec![27, 123, 3, 64, 64, 125]);
    tcp_data.append(&mut vec![27, 123, 7, 123, 0, 0, 83, 84, 34, 125]);
    tcp_data.append(&mut vec![27, 123, 7, 67, 2, 2, 1, 1, 73, 125]); // half-cut?
    tcp_data.append(&mut vec![27, 123, 4, 68, 5, 73, 125]);
    tcp_data.append(&mut vec![27, 123, 3, 71, 71, 125]);

    let mut tape_len_bytes = (td.width as u32 + 4/* safe margin */)
        .to_le_bytes()
        .to_vec();
    let mut cmd_bytes = vec![76];
    cmd_bytes.append(&mut tape_len_bytes);
    let csum = cmd_bytes
        .iter()
        .map(|v| Wrapping(*v))
        .sum::<Wrapping<u8>>()
        .0;
    cmd_bytes.push(csum);
    cmd_bytes.push(0x7d);
    tcp_data.append(&mut vec![0x1b, 0x7b, cmd_bytes.len() as u8]);
    tcp_data.append(&mut cmd_bytes);

    tcp_data.append(&mut vec![27, 123, 5, 84, 42, 0, 126, 125]);
    tcp_data.append(&mut vec![27, 123, 4, 72, 5, 77, 125]);
    tcp_data.append(&mut vec![27, 123, 4, 115, 0, 115, 125]);

    let row_bytes = (td.height + 7) / 8;
    for y in 0..td.width {
        tcp_data.append(&mut vec![0x1b, 0x2e, 0, 0, 0, 1]);
        tcp_data.append(&mut (td.height as u16).to_le_bytes().to_vec());
        for xb in 0..row_bytes {
            let mut chunk = 0x00;
            for dx in 0..8 {
                let x = xb * 8 + (7 - dx);

                if td.get_pixel(td.width - 1 - y, x) {
                    chunk |= 1 << dx
                }
            }
            tcp_data.push(chunk);
        }
    }
    tcp_data.push(0x0c); // data end
    tcp_data.append(&mut vec![27, 123, 3, 64, 64, 125]);
    Ok(tcp_data)
}

fn determine_tape_width_px(args: &PrintArgs) -> Result<i32> {
    let detected = if let Some(printer) = &args.printer {
        let socket = UdpSocket::bind("0.0.0.0:0").context("failed to bind")?;
        let info = StatusRequest::send(&socket, printer)?;
        eprintln!("Tape detected: {:?}", info);
        if let PrinterStatus::SomeTape(t) = info {
            Some(t)
        } else {
            eprintln!("Failed to detect tape width. status: {:?}", info);
            None
        }
    } else {
        None
    };
    let given = if let Some(mm) = args.width {
        Some(Tape::from_mm(mm)?)
    } else {
        None
    };
    Ok(match (given, detected) {
        (None, Some(w)) | (Some(w), None) => w,
        (Some(given), Some(detected)) => {
            if given != detected {
                eprintln!("Warning: {given:?} does not match with detected {detected:?}")
            }
            given
        }
        (None, None) => return Err(anyhow!("Please specify --width or --printer")),
    }
    .width_px())
}

fn print_qr_text(args: &PrintArgs) -> Result<()> {
    let text = args.qr_text.as_ref().expect("Please specify --qr-text");
    let tape_width_px = determine_tape_width_px(args)? as usize;
    let qr_td = {
        let mut td = TapeDisplay::new(tape_width_px, tape_width_px);
        let tape_width_px = tape_width_px as u32;
        let code = QrCode::new(text).unwrap();
        let image = code
            .render::<Luma<u8>>()
            .max_dimensions(tape_width_px, tape_width_px)
            .build();
        let ofs_x = (tape_width_px - image.width()) / 2;
        let ofs_y = (tape_width_px - image.height()) / 2;
        for (x, y, p) in image.enumerate_pixels() {
            Rectangle::new(
                Point::new((x + ofs_x) as i32, (y + ofs_y) as i32),
                Size::new_equal(1),
            )
            .draw_styled(
                &PrimitiveStyle::with_fill(BinaryColor::from(p.0[0] == 0)),
                &mut td,
            )?;
        }
        image.save("qrcode.png").unwrap();
        td
    };

    let mut td = TapeDisplay::new(qr_td.width, tape_width_px);
    td.overlay_or(&qr_td, 0, (td.height - qr_td.height) / 2);
    print_td(args, &td)
}

fn print_td(args: &PrintArgs, td: &TapeDisplay) -> Result<()> {
    // Generate preview image
    let path = Path::new(r"preview.png");
    let file = File::create(path).unwrap();
    let w = BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, td.width as u32, td.height as u32); // Width is 2 pixels and height is 1.
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_source_gamma(png::ScaledFloat::from_scaled(45455)); // 1.0 / 2.2, scaled by 100000
    encoder.set_source_gamma(png::ScaledFloat::new(1.0 / 2.2)); // 1.0 / 2.2, unscaled, but rounded
    let source_chromaticities = png::SourceChromaticities::new(
        // Using unscaled instantiation here
        (0.31270, 0.32900),
        (0.64000, 0.33000),
        (0.30000, 0.60000),
        (0.15000, 0.06000),
    );
    encoder.set_source_chromaticities(source_chromaticities);
    let mut writer = encoder.write_header().unwrap();
    let data: Vec<u8> = td
        .framebuffer
        .iter()
        .flat_map(|row| row.iter())
        .flat_map(|c| {
            // data will be [RGBARGBA...]
            if *c {
                [0, 0, 0, 255]
            } else {
                [255, 255, 255, 255]
            }
        })
        .collect();
    writer.write_image_data(&data).unwrap();

    let tcp_data = gen_tcp_data(td)?;

    if !args.dry_run {
        print_tcp_data(
            args.printer.as_ref().context("Please specify --printer")?,
            &tcp_data,
        )
    } else {
        analyze_tcp_data(&tcp_data)?;
        Ok(())
    }
}

#[derive(FromArgs, PartialEq, Debug)]
/// Print something
#[argh(subcommand, name = "print")]
pub struct PrintArgs {
    /// generate a label for a mac addr
    #[argh(option)]
    mac_addr: Option<String>,
    /// generate a label for a QR code with text
    #[argh(option)]
    qr_text: Option<String>,
    /// tape width in mm (default: auto)
    #[argh(option)]
    width: Option<usize>,
    /// do not print (just generate and analyze)
    #[argh(switch)]
    dry_run: bool,
    /// the raw dump of the TCP stream while printing
    #[argh(option)]
    tcp_data: Option<String>,
    /// an IPv4 address for the printer
    #[argh(option)]
    printer: Option<String>,
}
pub fn do_print(args: &PrintArgs) -> Result<()> {
    print_qr_text(args)
}
