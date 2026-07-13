# Third-Party Notices

## MAA OCR resource pack

`resources/ocr` contains only the files currently required by the operator
name recognizer:

- `rec/inference.onnx`
- `rec/keys.txt`
- `tasks/tasks.json`

Source repository: `MaaAssistantArknights/MaaAssistantArknights`.
Source commit: `6ceabc3cfc7143b53342337065f3d055ae78bc8d`.
The source repository declares `AGPL-3.0-only`; its license text is retained
at `resources/ocr/LICENSE-AGPL-3.0.txt`. File provenance and hashes are kept
in `resources/ocr/SOURCE.md`. Distribution of this application must undergo a
separate license review before release.

## ArknightsCostBarRuler

`third_party/cost-bar-ruler` is a Git submodule of
`https://github.com/ZeroAd-06/ArknightsCostBarRuler.git`, pinned to commit
`c18c9c8351c2f952bec7ed5e2cae2104e806eca0`. Its own license files remain in
the submodule and apply to that dependency.

## Map tile projection and map data

The grid projection in `crates/app-core/src/recorder.rs` follows the public
implementation lineage documented by
[`yuanyan3060/Arknights-Tile-Pos`](https://github.com/yuanyan3060/Arknights-Tile-Pos).
That repository is MIT licensed and requests attribution with its GitHub URL.

```text
MIT License

Copyright (c) 2022 yuanyan3060

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

The stored fields of the current 12 by 8 static table and its camera vectors
match `resource/Arknights-Tile-Pos/level_crisis_v2_05-01-obt-crisis-v2-level_crisis_v2_05-01.json`
in MaaAssistantArknights commit `bad264489337d78882b3f93e0fb2cce4a3296f29`.
MaaAssistantArknights declares `AGPL-3.0-only`. The historical `prts-plus`
checkout also states that its map data came from MaaAssistantArknights and is
itself AGPL-3.0 licensed.

The previous repository history was removed during migration, so the exact
historical import path cannot be proved. Before public distribution, perform a
separate license review for the static table and the projection implementation,
including the potential PRTS+ AGPL derivative path.
