# Test fixtures

Small files used by `tests/corpus.rs`. The JPEG/PNG files are unmodified
originals from Wikimedia Commons (the SHA-1 matches the upload) and are free
licences, mostly public domain. `watermark-sd.png` is synthetic; see below.

| File | Source | Author | Licence | Expected |
| --- | --- | --- | --- | --- |
| `c2pa-source-type.png` | [File:100 DAYS 100 EDITS 1 logo.png](https://commons.wikimedia.org/wiki/File:100_DAYS_100_EDITS_1_logo.png) | User:TatjanaClimate with DALL-E | Public domain | C2PA digital source type + software agent |
| `c2pa-firefly.jpg` | [File:Adobe Firefly generated realistic portrait.jpg](https://commons.wikimedia.org/wiki/File:Adobe_Firefly_generated_realistic_portrait.jpg) | Adobe Firefly Image 3, prompted by Jiří Vedral | Public domain | C2PA software agent (`Adobe Firefly`) |
| `xmp-source-type.jpg` | [File:100 articles.jpg](https://commons.wikimedia.org/wiki/File:100_articles.jpg) | Gemini AI | Public domain | XMP digital source type |
| `xmp-meta.jpg` | [File:A hyper realistic image of a nigerian (7).jpg](https://commons.wikimedia.org/wiki/File:A_hyper_realistic_image_of_a_nigerian_(7).jpg) | Afanla idris | CC0 | XMP digital source type |
| `exif-user-comment.png` | [File:A lonely blue man curled up in the fetal position floats in nothingness.png](https://commons.wikimedia.org/wiki/File:A_lonely_blue_man_curled_up_in_the_fetal_position_floats_in_nothingness.png) | Cameron Butler | Public domain | EXIF `UserComment` |
| `ordinary.jpg` | [File:Example.jpg](https://commons.wikimedia.org/wiki/File:Example.jpg) | Jon Sullivan | Public domain | no signals |
| `watermark-sd.png` | generated locally | — | — | `StableDiffusionV1` DWT-DCT payload |

Commons SHA-1s of the copied files:

```
aa27860004da36ae9d70690ba61315a30fe01a3a  c2pa-source-type.png
60e67cc65bc21f6a7bf2209ad813db654ce36a13  c2pa-firefly.jpg
5c400152a7a49f41419537c48c4c5e73452db78a  xmp-source-type.jpg
0b17aa9129b78b8c5182d121ebf4bc5f9eccc604  xmp-meta.jpg
d901f0a3881061e2136295ccb806ed666b037689  exif-user-comment.png
d01b79a6781c72ac9bfff93e5e2cfbeef4efc840  ordinary.jpg
```

## `watermark-sd.png`

Generated from a deterministic textured image with the reference Python
`invisible-watermark` package (`dwtDct`, payload `StableDiffusionV1`, 136 bits),
then saved as PNG. It is not a Commons file; it exists because the Commons
sample of AI-generated media contained no surviving DWT-DCT marks (the
reference Python decoder also finds none in those files, so this is not a gap
in our decoder). The test asserts that the CLI reports a high-confidence
watermark signal.

Do not commit the TrustMark ONNX models; `scripts/fetch-models.sh` downloads
them into the gitignored `models/` directory.

## Regenerating the Commons part

`audits/fetch_corpus.py` (gitignored scratch) lists and downloads files from
`Category:AI-generated_media` subcategories using the Commons API, politely and
with `maxlag`. `audits/run_corpus.py` then runs `aimer scan`/`calibrate` and
writes `audits/corpus/results.json`.
