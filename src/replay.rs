//! Rust translation of the argument-parsing contract from fx's cli_replay.zig.

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub path: String,
    pub frames: bool,
    pub json: bool,
    pub golden_path: Option<String>,
    pub frames_dir: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    MissingTapePath,
    TooManyArgs,
    UnknownFlag,
    MissingGoldenPath,
    MissingFramesDirPath,
}

pub fn parse_args(args: &[String]) -> Result<Options, ParseError> {
    let mut options = Options {
        path: String::new(),
        frames: false,
        json: false,
        golden_path: None,
        frames_dir: None,
    };
    let mut positional_seen = false;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--frames" => options.frames = true,
            "--json" => options.json = true,
            "--golden" => {
                index += 1;
                let value = args.get(index).ok_or(ParseError::MissingGoldenPath)?;
                options.golden_path = Some(value.clone());
            }
            "--frames-dir" => {
                index += 1;
                let value = args.get(index).ok_or(ParseError::MissingFramesDirPath)?;
                options.frames_dir = Some(value.clone());
            }
            value if value.starts_with("--") => return Err(ParseError::UnknownFlag),
            value => {
                if positional_seen {
                    return Err(ParseError::TooManyArgs);
                }
                options.path = value.to_owned();
                positional_seen = true;
            }
        }
        index += 1;
    }

    if !positional_seen {
        return Err(ParseError::MissingTapePath);
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn requires_tape_path() {
        assert_eq!(parse_args(&[]), Err(ParseError::MissingTapePath));
    }

    #[test]
    fn accepts_supported_flags() {
        let parsed = parse_args(&args(&[
            "--frames",
            "t.fxtape",
            "--json",
            "--golden",
            "out.txt",
            "--frames-dir",
            "frames",
        ]))
        .unwrap();
        assert_eq!(parsed.path, "t.fxtape");
        assert!(parsed.frames && parsed.json);
        assert_eq!(parsed.golden_path.as_deref(), Some("out.txt"));
        assert_eq!(parsed.frames_dir.as_deref(), Some("frames"));
    }

    #[test]
    fn rejects_invalid_forms() {
        assert_eq!(parse_args(&args(&["a", "b"])), Err(ParseError::TooManyArgs));
        assert_eq!(
            parse_args(&args(&["a", "--unknown"])),
            Err(ParseError::UnknownFlag)
        );
        assert_eq!(
            parse_args(&args(&["a", "--golden"])),
            Err(ParseError::MissingGoldenPath)
        );
        assert_eq!(
            parse_args(&args(&["a", "--frames-dir"])),
            Err(ParseError::MissingFramesDirPath)
        );
    }
}
