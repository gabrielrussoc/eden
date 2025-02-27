/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <unordered_map>

#include "eden/fs/model/Blob.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/model/RootId.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/store/BlobMetadata.h"
#include "eden/fs/store/IObjectStore.h"
#include "eden/fs/store/ImportPriority.h"
#include "eden/fs/store/ObjectFetchContext.h"

namespace facebook {
namespace eden {

/**
 * Fake implementation of IObjectStore that allows the data to be injected
 * directly. This is designed to be used for unit tests.
 */
class FakeObjectStore final : public IObjectStore {
 public:
  FakeObjectStore();
  ~FakeObjectStore() override;

  void addTree(Tree&& tree);
  void addBlob(Blob&& blob);
  void setTreeForCommit(const RootId& commitID, Tree&& tree);

  folly::Future<std::shared_ptr<const Tree>> getRootTree(
      const RootId& commitID,
      ObjectFetchContext& context =
          ObjectFetchContext::getNullContext()) const override;
  ImmediateFuture<std::shared_ptr<const Tree>> getTree(
      const ObjectId& id,
      ObjectFetchContext& context =
          ObjectFetchContext::getNullContext()) const override;
  folly::Future<std::shared_ptr<const Blob>> getBlob(
      const ObjectId& id,
      ObjectFetchContext& context =
          ObjectFetchContext::getNullContext()) const override;
  folly::Future<folly::Unit> prefetchBlobs(
      ObjectIdRange ids,
      ObjectFetchContext& context =
          ObjectFetchContext::getNullContext()) const override;

  size_t getAccessCount(const ObjectId& hash) const;

 private:
  std::unordered_map<RootId, Tree> commits_;
  std::unordered_map<ObjectId, Tree> trees_;
  std::unordered_map<ObjectId, Blob> blobs_;
  mutable std::unordered_map<RootId, size_t> commitAccessCounts_;
  mutable std::unordered_map<ObjectId, size_t> accessCounts_;
};
} // namespace eden
} // namespace facebook
