{
  lib,
  rustPlatform,
  makeWrapper,
  yt-dlp,
  ffmpeg-headless,
  gallery-dl,
}:
let
  root = ../.;
in
rustPlatform.buildRustPackage {
  pname = "mealie-forager";
  version = "0.1.0";

  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (root + "/Cargo.toml")
      (root + "/Cargo.lock")
      (root + "/src")
      (root + "/migrations")
      (root + "/web")
    ];
  };

  cargoLock.lockFile = root + "/Cargo.lock";

  nativeBuildInputs = [ makeWrapper ];

  postInstall = ''
    wrapProgram $out/bin/mealie-forager \
      --prefix PATH : ${
        lib.makeBinPath [
          yt-dlp
          ffmpeg-headless
          gallery-dl
        ]
      }
  '';

  meta = {
    description = "Queue-based importer that turns social media recipe posts into Mealie recipes";
    license = lib.licenses.mit;
    mainProgram = "mealie-forager";
  };
}
