use horizon_core::position::BlockPos;
use horizon_world::{
    chunk::{BlockState, ChunkData},
    compression::Compression,
    nbt::{self, Limits},
    region::{ChunkRecord, RegionFile, RegionWriter},
};
use std::{
    collections::BTreeMap,
    error::Error,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

fn codec(name: &str) -> Result<Compression, Box<dyn Error>> {
    match name {
        "gzip" | "1" => Ok(Compression::Gzip),
        "zlib" | "2" => Ok(Compression::Zlib),
        "none" | "3" => Ok(Compression::None),
        "lz4" | "4" => Ok(Compression::Lz4),
        "zstd" | "100" => Ok(Compression::Zstd),
        _ => Err(format!("unknown codec {name}; choose gzip, zlib, none, lz4 or zstd").into()),
    }
}

fn read_bounded(path: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let max = Limits::default().max_bytes;
    let mut bytes = Vec::new();
    File::open(path)?
        .take((max + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err("input exceeds 64 MiB limit".into());
    }
    Ok(bytes)
}

fn write_new(path: &str, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}

fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err("use --help for commands".into());
    };
    match command {
        "--help" | "-h" if args.len() == 1 => println!(
            "Horizon world data tools — Java Edition {}\n\
             inspect <region.mca>\n\
             copy-region <source.mca> <new-directory/r.x.z.mca> [gzip|zlib|none|lz4]\n\
             get-block <region.mca> <x> <y> <z>\n\
             edit-block-data <source.mca> <new-directory/r.x.z.mca> <x> <y> <z> <name> [property=value ...]\n\
             nbt-copy <input> <new-output> <input-codec> <output-codec>\n\
             nbt-network-copy <input> <new-output> <input-codec> <output-codec>\n\
             convert-world <world> hzr|mca   (Horizon's storage and back; docs/horizon-storage.md)\n\
             verify <world> [--repair]       (read every chunk; --repair writes back what parity rebuilt)\n\
             world-size <world>\n\
             Run the world commands on a world no game has open.\n\
             Existing output files are refused. edit-block-data changes saved block data only;\n\
             it does not run lighting, heightmaps, ticks or neighbor updates.",
            horizon_core::VANILLA_VERSION
        ),
        "convert-world" if args.len() == 3 => {
            use horizon_world::region_convert::{Target, convert_world};
            let target = match args[2].as_str() {
                "hzr" => Target::Hzr,
                "mca" => Target::Mca,
                other => return Err(format!("unknown format {other}; choose hzr or mca").into()),
            };
            let started = std::time::Instant::now();
            let report = convert_world(Path::new(&args[1]), target, |folder| {
                eprintln!("converting {folder}")
            })?;
            let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
            let change = if report.bytes_before == 0 {
                0.0
            } else {
                (report.bytes_after as f64 / report.bytes_before as f64 - 1.0) * 100.0
            };
            println!(
                "{} chunks in {} folders: {:.1} MiB -> {:.1} MiB ({change:+.1}%) in {:.1} s",
                report.chunks,
                report.folders,
                mib(report.bytes_before),
                mib(report.bytes_after),
                started.elapsed().as_secs_f64()
            );
        }
        "verify" if args.len() == 2 || (args.len() == 3 && args[2] == "--repair") => {
            let repair = args.len() == 3;
            let health = horizon_world::region_convert::verify_world(
                Path::new(&args[1]),
                repair,
                |folder| eprintln!("checking {folder}"),
            )?;
            for (file, chunk, why) in &health.bad {
                println!(
                    "BAD {} chunk {} {}: {why}",
                    file.display(),
                    chunk.x,
                    chunk.z
                );
            }
            println!(
                "{} files, {} chunks: {} bad; {} damaged blocks, {} repaired{}",
                health.files,
                health.chunks,
                health.bad.len(),
                health.damaged_blocks,
                health.repaired_blocks,
                if !repair && health.damaged_blocks > 0 {
                    " (run with --repair to write them back)"
                } else {
                    ""
                }
            );
            if !health.bad.is_empty() {
                return Err("some chunks can't be read".into());
            }
        }
        "bench-read" if args.len() == 2 => {
            let started = std::time::Instant::now();
            let n = horizon_world::region_convert::read_all(Path::new(&args[1]))?;
            let secs = started.elapsed().as_secs_f64();
            println!(
                "{n} chunks read and parsed in {secs:.2} s ({:.0} chunks/s)",
                n as f64 / secs
            );
        }
        "bench-write" if args.len() == 4 && (args[3] == "hzr" || args[3] == "mca") => {
            let dest = Path::new(&args[2]);
            if dest.exists() {
                return Err("the destination exists; name a new folder".into());
            }
            let (n, secs, bytes) = horizon_world::region_convert::write_all(
                Path::new(&args[1]),
                dest,
                args[3] == "hzr",
            )?;
            println!(
                "{n} chunks written as {} in {secs:.2} s ({:.0} chunks/s), {:.1} MiB",
                args[3],
                n as f64 / secs,
                bytes as f64 / (1024.0 * 1024.0)
            );
        }
        "world-size" if args.len() == 2 => {
            let (mca, hot, cold) = horizon_world::region_convert::world_size(Path::new(&args[1]))?;
            let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
            println!(
                "mca: {:.1} MiB\nhzr hot: {:.1} MiB\nhzr cold: {:.1} MiB",
                mib(mca),
                mib(hot),
                mib(cold)
            );
        }
        "nbt-copy" | "nbt-network-copy" if args.len() == 5 => {
            let bytes =
                codec(&args[3])?.decode(&read_bounded(&args[1])?, Limits::default().max_bytes)?;
            let encoded = if command == "nbt-copy" {
                let (root, used) = nbt::read_named(&bytes, Limits::default())?;
                if used != bytes.len() {
                    return Err("trailing bytes after NBT root".into());
                }
                nbt::write_named(&root, Limits::default())?
            } else {
                let (root, used) = nbt::read_network(&bytes, Limits::default())?;
                if used != bytes.len() {
                    return Err("trailing bytes after NBT root".into());
                }
                nbt::write_network(&root, Limits::default())?
            };
            write_new(&args[2], &codec(&args[4])?.encode(&encoded)?)?;
            println!("Wrote {}", args[2]);
        }
        "inspect" if args.len() == 2 => {
            let mut region = RegionFile::open(&args[1])?;
            let chunks: Vec<_> = region.chunks().collect();
            println!(
                "Horizon region {},{}: {} chunks",
                region.position().x,
                region.position().z,
                chunks.len()
            );
            for (pos, timestamp) in chunks {
                let record = region.read(pos)?.unwrap();
                let root = record.nbt(Limits::default())?;
                println!(
                    "chunk {},{} timestamp={} codec={:?} compressed={} root_type={}",
                    pos.x,
                    pos.z,
                    timestamp,
                    record.compression,
                    record.compressed.len(),
                    root.tag.id()
                );
            }
        }
        "copy-region" if args.len() == 3 || args.len() == 4 => {
            let mut region = RegionFile::open(&args[1])?;
            if args.len() == 3 {
                region.export(&args[2], &[])?;
            } else {
                let compression = codec(&args[3])?;
                if horizon_world::region::position_from_path(Path::new(&args[2]))?
                    != region.position()
                {
                    return Err("export region coordinates differ".into());
                }
                let mut writer = RegionWriter::create(&args[2])?;
                let positions: Vec<_> = region.chunks().map(|(p, _)| p).collect();
                for pos in positions {
                    let old = region.read(pos)?.unwrap();
                    let record = ChunkRecord::from_nbt(
                        &old.nbt(Limits::default())?,
                        compression,
                        old.timestamp,
                        Limits::default(),
                    )?;
                    writer.write(pos, &record)?;
                }
                writer.finish()?;
            }
            println!("Wrote {}", args[2]);
        }
        "get-block" if args.len() == 5 => {
            let block = BlockPos::new(args[2].parse()?, args[3].parse()?, args[4].parse()?);
            let record = RegionFile::open(&args[1])?
                .read(block.chunk())?
                .ok_or("chunk is absent")?;
            let chunk = ChunkData::new(record.nbt(Limits::default())?)?;
            let state = chunk
                .block(block)?
                .ok_or("section has no block-state data")?;
            print!("{}", state.name);
            for (name, value) in state.properties {
                print!(" {name}={value}");
            }
            println!();
        }
        "edit-block-data" if args.len() >= 7 => {
            let block = BlockPos::new(args[3].parse()?, args[4].parse()?, args[5].parse()?);
            let mut properties = BTreeMap::new();
            for arg in &args[7..] {
                let (key, value) = arg.split_once('=').ok_or("expected property=value")?;
                if key.is_empty()
                    || value.is_empty()
                    || properties
                        .insert(key.to_owned(), value.to_owned())
                        .is_some()
                {
                    return Err("invalid or duplicate block property".into());
                }
            }
            let mut region = RegionFile::open(&args[1])?;
            let old = region.read(block.chunk())?.ok_or("chunk is absent")?;
            let mut chunk = ChunkData::new(old.nbt(Limits::default())?)?;
            chunk.set_block_data(
                block,
                BlockState {
                    name: args[6].clone(),
                    properties: properties.into(),
                },
            )?;
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as u32;
            let next = ChunkRecord::from_nbt(
                &chunk.into_nbt(),
                old.compression,
                timestamp,
                Limits::default(),
            )?;
            region.export(&args[2], &[(block.chunk(), Some(next))])?;
            println!(
                "Wrote saved block data to {}; simulation-derived data has not been updated.",
                args[2]
            );
        }
        _ => return Err("invalid command or arguments; use --help".into()),
    }
    Ok(())
}

fn main() {
    if let Err(error) = run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        eprintln!("Horizon: {error}");
        std::process::exit(2);
    }
}
