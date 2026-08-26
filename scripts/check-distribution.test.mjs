import assert from 'node:assert/strict'
import test from 'node:test'

import { validateDistribution } from './lib/distribution-policy.mjs'

test('accepts an empty public runtime bundle', () => {
  assert.doesNotThrow(() =>
    validateDistribution({ bundle: { externalBin: [] } }, { sidecars: [] }),
  )
})

test('rejects a fake runner configured as an external binary', () => {
  assert.throws(
    () =>
      validateDistribution(
        { bundle: { externalBin: ['binaries/zoos-runner-fake'] } },
        { sidecars: [] },
      ),
    /unapproved runtime asset/,
  )
})

test('rejects the ORT wrapper and runtime from the public bundle', () => {
  assert.throws(
    () =>
      validateDistribution(
        { bundle: { externalBin: ['binaries/zoos-runner-ort'] } },
        { sidecars: [] },
      ),
    /unapproved runtime asset/,
  )
  assert.throws(
    () =>
      validateDistribution(
        { bundle: { resources: ['lib/libonnxruntime.1.29.0.dylib'] } },
        { sidecars: [] },
      ),
    /unapproved runtime asset/,
  )
})

test('rejects FFmpeg and RIFE assets from the public bundle', () => {
  for (const asset of ['bin/ffmpeg', 'bin/ffprobe', 'binaries/zoos-runner-rife', 'bin/rife-ncnn-vulkan']) {
    assert.throws(
      () => validateDistribution({ bundle: { resources: [asset] } }, { sidecars: [] }),
      /unapproved runtime asset/,
    )
  }
})

test('rejects a model configured as a public resource', () => {
  assert.throws(
    () =>
      validateDistribution(
        { bundle: { resources: ['models/photo.bin'] } },
        { sidecars: [] },
      ),
    /unapproved runtime asset/,
  )
})

test('rejects an unapproved sidecar marked for release', () => {
  assert.throws(
    () =>
      validateDistribution(
        { bundle: {} },
        {
          sidecars: [
            {
              id: 'future-runner',
              bundled_in_release: true,
              approved_for_distribution: false,
            },
          ],
        },
      ),
    /approved_for_distribution=true/,
  )
})

test('rejects an unapproved catalog marked for release', () => {
  assert.throws(
    () =>
      validateDistribution(
        { bundle: {} },
        { sidecars: [] },
        [
          {
            id: 'model-package',
            bundled_in_release: true,
            approved_for_distribution: false,
          },
        ],
      ),
    /approved_for_distribution=true/,
  )
})
