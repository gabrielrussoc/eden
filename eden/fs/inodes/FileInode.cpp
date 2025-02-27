/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/FileInode.h"

#include <folly/io/Cursor.h>
#include <folly/io/IOBuf.h>
#include <folly/io/async/EventBase.h>
#include <folly/logging/xlog.h>
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/InodeError.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/Blob.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/store/BlobMetadata.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/telemetry/IHiveLogger.h"
#include "eden/fs/utils/Bug.h"
#include "eden/fs/utils/Clock.h"
#include "eden/fs/utils/DirType.h"
#include "eden/fs/utils/EnumValue.h"
#include "eden/fs/utils/FileHash.h"
#include "eden/fs/utils/FileUtils.h"
#include "eden/fs/utils/UnboundedQueueExecutor.h"

#ifndef _WIN32
#include "eden/fs/inodes/InodeTable.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/store/BlobAccess.h"
#include "eden/fs/utils/XAttr.h"
#endif

using folly::Future;
using folly::makeFuture;
using folly::StringPiece;
using std::string;
using std::vector;

namespace facebook {
namespace eden {

/*********************************************************************
 * FileInode::LockedState
 ********************************************************************/

/**
 * LockedState is a helper class that wraps
 * folly::Synchronized<State>::LockedPtr
 *
 * It implements operator->() and operator*() so it can be used just like
 * LockedPtr.
 */
class FileInode::LockedState {
 public:
  explicit LockedState(FileInode* inode) : ptr_{inode->state_.wlock()} {}
  explicit LockedState(const FileInodePtr& inode)
      : ptr_{inode->state_.wlock()} {}

  LockedState(LockedState&&) = default;
  LockedState& operator=(LockedState&&) = default;

  ~LockedState();

  State* operator->() const {
    return ptr_.operator->();
  }
  State& operator*() const {
    return ptr_.operator*();
  }

  bool isNull() const {
    return ptr_.isNull();
  }
  explicit operator bool() const {
    return !ptr_.isNull();
  }

  /**
   * Explicitly unlock the LockedState object before it is destroyed.
   */
  void unlock();

  /**
   * Move the file into the MATERIALIZED_IN_OVERLAY state.
   *
   * This updates state->tag and state->nonMaterializedState.
   */
  void setMaterialized();

  /**
   * If this inode still has access to a cached blob, return it.
   *
   * Can only be called when not materialized.
   */
  std::shared_ptr<const Blob> getCachedBlob(
      EdenMount* mount,
      BlobCache::Interest interest);

 private:
  folly::Synchronized<State>::LockedPtr ptr_;
};

FileInode::LockedState::~LockedState() {
  if (!ptr_) {
    return;
  }
  // Check the state invariants every time we release the lock
  ptr_->checkInvariants();
}

void FileInode::LockedState::unlock() {
  ptr_->checkInvariants();
  ptr_.unlock();
}

std::shared_ptr<const Blob> FileInode::LockedState::getCachedBlob(
    EdenMount* mount,
    BlobCache::Interest interest) {
  XCHECK(!ptr_->isMaterialized())
      << "getCachedBlob can only be called when not materialized";

  // Is the previous handle still valid? If so, return it.
  if (auto blob = ptr_->interestHandle.getObject()) {
    return blob;
  }
  // Otherwise, does the cache have one?
  //
  // The BlobAccess::getBlob call in startLoadingData on a cache miss will also
  // check the BlobCache, but by checking it here, we can avoid a transition to
  // BLOB_LOADING and back, and also avoid allocating some futures and closures.
  auto result =
      mount->getBlobCache()->get(ptr_->nonMaterializedState->hash, interest);
  if (result.object) {
    ptr_->interestHandle = std::move(result.interestHandle);
    return std::move(result.object);
  }

  // If we received a read and missed cache because the blob was
  // already evicted, assume the existing readByteRanges CoverageSet
  // doesn't accurately reflect how much data is in the kernel's
  // caches.
  ptr_->interestHandle.reset();
#ifndef _WIN32
  ptr_->readByteRanges.clear();
#endif // !_WIN32

  return nullptr;
}

void FileInode::LockedState::setMaterialized() {
  ptr_->nonMaterializedState.reset();
  ptr_->tag = State::MATERIALIZED_IN_OVERLAY;

  ptr_->interestHandle.reset();

#ifndef _WIN32
  ptr_->readByteRanges.clear();
#endif
}

/*********************************************************************
 * Implementations of FileInode private template methods
 * These definitions need to appear before any functions that use them.
 ********************************************************************/

template <typename ReturnType, typename Fn>
ReturnType FileInode::runWhileDataLoaded(
    LockedState state,
    BlobCache::Interest interest,
    ObjectFetchContext& fetchContext,
    std::shared_ptr<const Blob> blob,
    Fn&& fn) {
  auto future = Future<std::shared_ptr<const Blob>>::makeEmpty();
  switch (state->tag) {
    case State::BLOB_NOT_LOADING:
      if (!blob) {
        // If no blob is given, check cache.
        blob = state.getCachedBlob(getMount(), interest);
      }
      if (blob) {
        logAccess(fetchContext);
        // The blob was still in cache, so we can run the function immediately.
        return folly::makeFutureWith([&] {
          return std::forward<Fn>(fn)(std::move(state), std::move(blob));
        });
      } else {
        future = startLoadingData(std::move(state), interest, fetchContext);
      }
      break;
    case State::BLOB_LOADING:
      // If we're already loading, latch on to the in-progress load
      future = state->blobLoadingPromise->getFuture();
      state.unlock();
      break;
    case State::MATERIALIZED_IN_OVERLAY:
      logAccess(fetchContext);
      return folly::makeFutureWith(
          [&] { return std::forward<Fn>(fn)(std::move(state), nullptr); });
  }

  return std::move(future).thenValue(
      [self = inodePtrFromThis(),
       fn = std::forward<Fn>(fn),
       interest,
       &fetchContext](std::shared_ptr<const Blob> blob) mutable {
        // Simply call runWhileDataLoaded() again when we we finish loading the
        // blob data.  The state should be BLOB_NOT_LOADING or
        // MATERIALIZED_IN_OVERLAY this time around.
        auto stateLock = LockedState{self};
        XDCHECK(
            stateLock->tag == State::BLOB_NOT_LOADING ||
            stateLock->tag == State::MATERIALIZED_IN_OVERLAY)
            << "unexpected FileInode state after loading: " << stateLock->tag;
        return self->runWhileDataLoaded<ReturnType>(
            std::move(stateLock),
            interest,
            fetchContext,
            std::move(blob),
            std::forward<Fn>(fn));
      });
}

#ifndef _WIN32
template <typename Fn>
typename folly::futures::detail::callableResult<FileInode::LockedState, Fn>::
    Return
    FileInode::runWhileMaterialized(
        LockedState state,
        std::shared_ptr<const Blob> blob,
        Fn&& fn,
        ObjectFetchContext& fetchContext) {
  auto future = Future<std::shared_ptr<const Blob>>::makeEmpty();
  switch (state->tag) {
    case State::BLOB_NOT_LOADING:
      if (!blob) {
        // If no blob is given, check cache.
        blob = state.getCachedBlob(
            getMount(), BlobCache::Interest::UnlikelyNeededAgain);
      }
      if (blob) {
        // We have the blob data loaded.
        // Materialize the file now.
        materializeNow(state, blob, fetchContext);
        // Call materializeInParent before we return, after we are
        // sure the state lock has been released.  This does mean that our
        // parent won't have updated our state until after the caller's function
        // runs, but this is okay.  There is always a brief gap between when we
        // materialize ourself and when our parent gets updated to indicate
        // this. If we do crash during this period it is not too unreasonable
        // that recent change right before the crash might be reverted to their
        // non-materialized state.
        SCOPE_EXIT {
          XCHECK(state.isNull());
          materializeInParent();
        };
        logAccess(fetchContext);
        // Note that we explicitly create a temporary LockedState object
        // to pass to the caller to ensure that the state lock will be released
        // when they return, even if the caller's function accepts the state as
        // an rvalue-reference and does not release it themselves.
        return folly::makeFutureWith([&] {
          return std::forward<Fn>(fn)(LockedState{std::move(state)});
        });
      }

      // The blob must be loaded, so kick that off. There's no point in caching
      // it in memory - the blob will immediately be written into the overlay
      // and then dropped.
      future = startLoadingData(
          std::move(state),
          BlobCache::Interest::UnlikelyNeededAgain,
          fetchContext);
      break;
    case State::BLOB_LOADING:
      // If we're already loading, latch on to the in-progress load
      future = state->blobLoadingPromise->getFuture();
      state.unlock();
      break;
    case State::MATERIALIZED_IN_OVERLAY:
      logAccess(fetchContext);
      return folly::makeFutureWith(
          [&] { return std::forward<Fn>(fn)(LockedState{std::move(state)}); });
  }

  return std::move(future).thenValue(
      [self = inodePtrFromThis(), fn = std::forward<Fn>(fn), &fetchContext](
          std::shared_ptr<const Blob> blob) mutable {
        // Simply call runWhileMaterialized() again when we we are finished
        // loading the blob data.
        auto stateLock = LockedState{self};
        XDCHECK(
            stateLock->tag == State::BLOB_NOT_LOADING ||
            stateLock->tag == State::MATERIALIZED_IN_OVERLAY)
            << "unexpected FileInode state after loading: " << stateLock->tag;
        return self->runWhileMaterialized(
            std::move(stateLock),
            std::move(blob),
            std::forward<Fn>(fn),
            fetchContext);
      });
}

template <typename Fn>
typename std::result_of<Fn(FileInode::LockedState&&)>::type
FileInode::truncateAndRun(LockedState state, Fn&& fn) {
  switch (state->tag) {
    case State::BLOB_NOT_LOADING:
    case State::BLOB_LOADING: {
      // We are not materialized yet.  We need to materialize the file now.
      //
      // Note that we have to be pretty careful about ordering of operations
      // here and how we behave if an exception is thrown at any point.  We
      // want to:
      // - Truncate the file.
      // - Invoke the input function with the state lock still held.
      // - Release the state lock
      // - Assuming we successfully materialized the file, mark ourself
      //   materialized in our parent TreeInode.
      // - If we successfully materialized the file and were in the
      //   BLOB_LOADING state, fulfill the blobLoadingPromise.
      std::unique_ptr<folly::SharedPromise<std::shared_ptr<const Blob>>>
          loadingPromise;
      SCOPE_EXIT {
        if (loadingPromise) {
          // If transitioning from the loading state to materialized, fulfill
          // the loading promise will null. Callbacks will have to handle the
          // case that the state is now materialized.
          loadingPromise->setValue(nullptr);
        }
      };

      // Call materializeAndTruncate()
      materializeAndTruncate(state);

      // Now that materializeAndTruncate() has succeeded, extract the
      // blobLoadingPromise so we can fulfill it as we exit.
      loadingPromise = std::move(state->blobLoadingPromise);
      state->blobLoadingPromise.reset();
      // Also call materializeInParent() as we exit, before fulfilling the
      // blobLoadingPromise.
      SCOPE_EXIT {
        XCHECK(state.isNull());
        materializeInParent();
      };

      // Now invoke the input function.
      // Note that we explicitly create a temporary LockedState object
      // to pass to the caller to ensure that the state lock will be released
      // when they return, even if the caller's function accepts the state as
      // an rvalue-reference and does not release it themselves.
      return std::forward<Fn>(fn)(LockedState{std::move(state)});
    }
    case State::MATERIALIZED_IN_OVERLAY:
      // We are already materialized.
      // Truncate the file in the overlay, then call the function.
      truncateInOverlay(state);
      return std::forward<Fn>(fn)(std::move(state));
  }

  XLOG(FATAL) << "unexpected FileInode state " << state->tag;
}
#endif // !_WIN32

/*********************************************************************
 * FileInode::State methods
 ********************************************************************/

FileInodeState::FileInodeState(const std::optional<ObjectId>& h)
    : nonMaterializedState(
          h ? std::optional(NonMaterializedState{*h}) : std::nullopt) {
  tag = nonMaterializedState ? BLOB_NOT_LOADING : MATERIALIZED_IN_OVERLAY;

  checkInvariants();
}

FileInodeState::FileInodeState() : tag(MATERIALIZED_IN_OVERLAY) {
  checkInvariants();
}

/*
 * Define FileInodeState destructor explicitly to avoid including
 * some header files in FileInode.h
 */
FileInodeState::~FileInodeState() = default;

void FileInodeState::checkInvariants() {
  switch (tag) {
    case BLOB_NOT_LOADING:
      XCHECK(nonMaterializedState);
      XCHECK(!blobLoadingPromise);
      return;
    case BLOB_LOADING:
      XCHECK(nonMaterializedState);
      XCHECK(blobLoadingPromise);
#ifndef _WIN32
      XCHECK(readByteRanges.empty());
#endif
      return;
    case MATERIALIZED_IN_OVERLAY:
      // 'materialized'
      XCHECK(!nonMaterializedState);
      XCHECK(!blobLoadingPromise);
#ifndef _WIN32
      XCHECK(readByteRanges.empty());
#endif
      return;
  }

  XLOG(FATAL) << "Unexpected tag value: " << tag;
}

/*********************************************************************
 * FileInode methods
 ********************************************************************/

// The FileInode is in NOT_LOADED or MATERIALIZED_IN_OVERLAY state.
FileInode::FileInode(
    InodeNumber ino,
    TreeInodePtr parentInode,
    PathComponentPiece name,
    mode_t initialMode,
    const std::optional<InodeTimestamps>& initialTimestamps,
    const std::optional<ObjectId>& hash)
    : Base(ino, initialMode, initialTimestamps, std::move(parentInode), name),
      state_(folly::in_place, hash) {}

// The FileInode is in MATERIALIZED_IN_OVERLAY state.
FileInode::FileInode(
    InodeNumber ino,
    TreeInodePtr parentInode,
    PathComponentPiece name,
    mode_t initialMode,
    const InodeTimestamps& initialTimestamps)
    : Base(ino, initialMode, initialTimestamps, std::move(parentInode), name),
      state_(folly::in_place) {}

#ifndef _WIN32
folly::Future<struct stat> FileInode::setattr(
    const DesiredMetadata& desired,
    ObjectFetchContext& fetchContext) {
  // If this file is inside of .eden it cannot be reparented, so getParentRacy()
  // is okay.
  auto parent = getParentRacy();
  if (parent && parent->getNodeId() == getMount()->getDotEdenInodeNumber()) {
    return folly::makeFuture<struct stat>(
        InodeError(EPERM, inodePtrFromThis()));
  }

  auto setAttrs = [self = inodePtrFromThis(), desired](LockedState&& state) {
    auto ino = self->getNodeId();
    auto result = self->getMount()->initStatData();

    XDCHECK_EQ(State::MATERIALIZED_IN_OVERLAY, state->tag)
        << "Must have a file in the overlay at this point";

    // Set the size of the file when desired.size is set
    if (desired.size.has_value()) {
      // Throws upon error.
      self->getOverlayFileAccess(state)->truncate(*self, desired.size.value());
    }

    auto metadata = self->getMount()->getInodeMetadataTable()->modifyOrThrow(
        ino, [&](auto& metadata) {
          metadata.updateFromDesired(self->getClock(), desired);
        });

    // We need to call fstat function here to get the size of the overlay
    // file. We might update size in the result while truncating the file
    // when desired.size flag is set but when the flag is not set we
    // have to return the correct size of the file even if some size is sent
    // in attr.st.st_size.
    off_t size = self->getOverlayFileAccess(state)->getFileSize(*self);
    result.st_ino = ino.get();
    result.st_size = size;
    metadata.applyToStat(result);
    result.st_nlink = 1;
    updateBlockCount(result);

    // Update the Journal
    self->updateJournal();
    return result;
  };

  // Minor optimization: if we know that the file is being completely truncated
  // as part of this operation, there's no need to fetch the underlying data,
  // so use truncateAndRun() rather than runWhileMaterialized()
  bool truncate = desired.size.has_value() && desired.size.value() == 0;
  auto state = LockedState{this};
  if (truncate) {
    return truncateAndRun(std::move(state), setAttrs);
  } else {
    return runWhileMaterialized(
        std::move(state), nullptr, setAttrs, fetchContext);
  }
}

folly::Future<std::string> FileInode::readlink(
    ObjectFetchContext& fetchContext,
    CacheHint cacheHint) {
  if (dtype_t::Symlink != getType()) {
    // man 2 readlink says:  EINVAL The named file is not a symbolic link.
    throw InodeError(EINVAL, inodePtrFromThis(), "not a symlink");
  }

  // The symlink contents are simply the file contents!
  return readAll(fetchContext, cacheHint);
}
#endif // !_WIN32

std::optional<bool> FileInode::isSameAsFast(
    const ObjectId& blobID,
    TreeEntryType entryType) {
  auto state = state_.rlock();
#ifndef _WIN32
  if (entryType != treeEntryTypeFromMode(getMetadataLocked(*state).mode)) {
    return false;
  }
#else
  // Note: the Windows-specific version of getMode() is safe to call here even
  // though we are holding the state_ lock.  On non-Windows getMetadataLocked()
  // must be used instead when holding the lock.
  if (entryType != treeEntryTypeFromMode(getMode())) {
    return false;
  }
#endif // !_WIN32

  if (state->nonMaterializedState.has_value()) {
    // This file is not materialized, so we can compare blob hashes.
    // If the hashes are the same then assume the contents are the same.
    //
    // Unfortunately we cannot assume that the file contents are different if
    // the hashes are different: Mercurial's blob hashes also include history
    // metadata, so there may be multiple different blob hashes for the same
    // file contents.
    if (state->nonMaterializedState->hash == blobID) {
      return true;
    }
  }
  return std::nullopt;
}

ImmediateFuture<bool> FileInode::isSameAsSlow(
    const Hash20& expectedBlobSha1,
    ObjectFetchContext& fetchContext) {
  return getSha1(fetchContext)
      .thenTry([expectedBlobSha1](folly::Try<Hash20>&& try_) {
        if (try_.hasException()) {
          XLOG(DBG2) << "Assuming changed: " << try_.exception();
          return false;
        } else {
          return try_.value() == expectedBlobSha1;
        }
      });
}

ImmediateFuture<bool> FileInode::isSameAs(
    const Blob& blob,
    TreeEntryType entryType,
    ObjectFetchContext& fetchContext) {
  auto result = isSameAsFast(blob.getHash(), entryType);
  if (result.has_value()) {
    return result.value();
  }

  auto blobSha1 = Hash20::sha1(blob.getContents());
  return isSameAsSlow(blobSha1, fetchContext);
}

ImmediateFuture<bool> FileInode::isSameAs(
    const ObjectId& blobID,
    const Hash20& blobSha1,
    TreeEntryType entryType,
    ObjectFetchContext& fetchContext) {
  auto result = isSameAsFast(blobID, entryType);
  if (result.has_value()) {
    return result.value();
  }

  return isSameAsSlow(blobSha1, fetchContext);
}

ImmediateFuture<bool> FileInode::isSameAs(
    const ObjectId& blobID,
    TreeEntryType entryType,
    ObjectFetchContext& fetchContext) {
  auto result = isSameAsFast(blobID, entryType);
  if (result.has_value()) {
    return result.value();
  }

  auto f1 = getSha1(fetchContext);
  auto f2 = getMount()->getObjectStore()->getBlobSha1(blobID, fetchContext);
  return collectAllSafe(f1, f2).thenTry(
      [](folly::Try<std::tuple<Hash20, Hash20>>&& try_) {
        if (try_.hasException()) {
          XLOG(DBG2) << "Assuming changed: " << try_.exception();
          return false;
        } else {
          auto hashes = std::move(try_).value();
          return std::get<0>(hashes) == std::get<1>(hashes);
        }
      });
}

#ifndef _WIN32
mode_t FileInode::getMode() const {
  return getMetadata().mode;
}

mode_t FileInode::getPermissions() const {
  return (getMode() & 07777);
}

InodeMetadata FileInode::getMetadata() const {
  auto lock = state_.rlock();
  return getMetadataLocked(*lock);
}

#else
mode_t FileInode::getMode() const {
  // On Windows we only store the dir type info and no permissions bits here.
  // For file it will always be a regular file.
  return _S_IFREG;
}
#endif // !_WIN32

std::optional<ObjectId> FileInode::getBlobHash() const {
  if (auto state = state_.rlock(); state->nonMaterializedState) {
    return state->nonMaterializedState->hash;
  } else {
    return std::nullopt;
  }
}

void FileInode::materializeInParent() {
  auto renameLock = getMount()->acquireRenameLock();
  auto loc = getLocationInfo(renameLock);
  if (loc.parent && !loc.unlinked) {
    loc.parent->childMaterialized(renameLock, loc.name);
  }
}

#ifndef _WIN32
ImmediateFuture<vector<string>> FileInode::listxattr() {
  vector<string> attributes;
  // We used to return kXattrSha1 here for regular files, but
  // that caused some annoying behavior with appledouble
  // metadata files being created by various tools that wanted
  // to preserve all of these attributes across copy on macos.
  // So now we just return an empty set on all systems.
  return attributes;
}

ImmediateFuture<string> FileInode::getxattr(
    StringPiece name,
    ObjectFetchContext& context) {
  // Currently, we only support the xattr for the SHA-1 of a regular file.
  if (name != kXattrSha1) {
    return makeImmediateFuture<string>(
        InodeError(kENOATTR, inodePtrFromThis()));
  }

  return getSha1(context).thenValue(
      [](Hash20 hash) { return hash.toString(); });
}
#else

AbsolutePath FileInode::getMaterializedFilePath() {
  auto filePath = getPath();
  if (!filePath.has_value()) {
    throw InodeError(
        EINVAL, inodePtrFromThis(), "File is unlinked", getLogPath());
  }
  return getMount()->getPath() + filePath.value();
}
#endif

ImmediateFuture<Hash20> FileInode::getSha1(ObjectFetchContext& fetchContext) {
  auto state = LockedState{this};

  logAccess(fetchContext);
  switch (state->tag) {
    case State::BLOB_NOT_LOADING:
    case State::BLOB_LOADING:
      // If a file is not materialized, it should have a hash value.
      return getObjectStore()->getBlobSha1(
          state->nonMaterializedState->hash, fetchContext);
    case State::MATERIALIZED_IN_OVERLAY:
#ifdef _WIN32
      return makeImmediateFutureWith(
          [this] { return getFileSha1(getMaterializedFilePath()); });
#else
      return getOverlayFileAccess(state)->getSha1(*this);
#endif // _WIN32
  }

  XLOG(FATAL) << "FileInode in illegal state: " << state->tag;
}

ImmediateFuture<struct stat> FileInode::stat(ObjectFetchContext& context) {
  auto st = getMount()->initStatData();
  st.st_nlink = 1; // Eden does not support hard links yet.
  st.st_ino = getNodeId().get();
  // NOTE: we don't set rdev to anything special here because we
  // don't support committing special device nodes.

  auto state = LockedState{this};

#ifndef _WIN32
  getMetadataLocked(*state).applyToStat(st);
#endif

  if (state->isMaterialized()) {
#ifdef _WIN32
    auto filePath = getPath();
    if (!filePath.has_value()) {
      throw InodeError(ENOENT, inodePtrFromThis(), "not a symlink");
    }
    AbsolutePath pathToFile = getMount()->getPath() + filePath.value();
    struct stat targetStat;
    if (::stat(pathToFile.c_str(), &targetStat) == 0) {
      st.st_size = targetStat.st_size;
    }
#else
    st.st_size = getOverlayFileAccess(state)->getFileSize(*this);
#endif
    updateBlockCount(st);
    return st;
  } else {
    XCHECK(state->nonMaterializedState.has_value());

    if (state->nonMaterializedState->size !=
        FileInodeState::NonMaterializedState::kUnknownSize) {
      st.st_size = state->nonMaterializedState->size;
      updateBlockCount(st);
      return st;
    }

    // While getBlobSize will sometimes need to fetch a blob to compute the
    // size, if it's already known, return the cached size. This is especially
    // a win after restarting Eden - size can be loaded from the local cache
    // more cheaply than deserializing an entire blob.
    auto sizeFut = getObjectStore()->getBlobSize(
        state->nonMaterializedState->hash, context);
    state.unlock();

    return std::move(sizeFut).thenValue(
        [self = inodePtrFromThis(), st](const uint64_t size) mutable {
          if (auto lockedState = LockedState{self};
              !lockedState->isMaterialized()) {
            lockedState->nonMaterializedState->size = size;
          }
          st.st_size = size;
          updateBlockCount(st);
          return st;
        });
  }
}

void FileInode::updateBlockCount(FOLLY_MAYBE_UNUSED struct stat& st) {
  // win32 does not have stat::st_blocks
#ifndef _WIN32
  // Compute a value to store in st_blocks based on st_size.
  // Note that st_blocks always refers to 512 byte blocks, regardless of the
  // value we report in st.st_blksize.
  static constexpr off_t kBlockSize = 512;
  st.st_blocks = ((st.st_size + kBlockSize - 1) / kBlockSize);
#endif
}

#ifndef _WIN32
void FileInode::fsync(bool datasync) {
  auto state = LockedState{this};
  if (state->isMaterialized()) {
    getOverlayFileAccess(state)->fsync(*this, datasync);
  }
}

folly::Future<folly::Unit> FileInode::fallocate(
    uint64_t offset,
    uint64_t length,
    ObjectFetchContext& fetchContext) {
  return runWhileMaterialized(
      LockedState{this},
      nullptr,
      [offset, length, self = inodePtrFromThis()](LockedState&& state) {
        self->getOverlayFileAccess(state)->fallocate(*self, offset, length);
      },
      fetchContext);
}
#endif

Future<string> FileInode::readAll(
    ObjectFetchContext& fetchContext,
    CacheHint cacheHint) {
  auto interest = BlobCache::Interest::LikelyNeededAgain;
  switch (cacheHint) {
    case CacheHint::NotNeededAgain:
      interest = BlobCache::Interest::UnlikelyNeededAgain;
      break;
    case CacheHint::LikelyNeededAgain:
      // readAll() with LikelyNeededAgain is primarily called for files read
      // by Eden itself, like .gitignore, and for symlinks on kernels that don't
      // cache readlink. At least keep the blob around while the inode is
      // loaded.
      interest = BlobCache::Interest::WantHandle;
      break;
  }

  return runWhileDataLoaded<Future<string>>(
      LockedState{this},
      interest,
      fetchContext,
      nullptr,
      [self = inodePtrFromThis()](
          LockedState&& state, std::shared_ptr<const Blob> blob) -> string {
        std::string result;
        switch (state->tag) {
          case State::MATERIALIZED_IN_OVERLAY: {
#ifdef _WIN32
            result = readFile(self->getMaterializedFilePath()).value();
#else
            XDCHECK(!blob);
            result = self->getOverlayFileAccess(state)->readAllContents(*self);
#endif
            break;
          }
          case State::BLOB_NOT_LOADING: {
            const auto& contentsBuf = blob->getContents();
            folly::io::Cursor cursor(&contentsBuf);
            result =
                cursor.readFixedString(contentsBuf.computeChainDataLength());
            break;
          }
          default:
            EDEN_BUG() << "neither materialized nor loaded during "
                          "runWhileDataLoaded() call";
        }

        // We want to update atime after the read operation.
        self->updateAtimeLocked(*state);

        return result;
      });
}

#ifdef _WIN32
void FileInode::materialize() {
  {
    auto state = LockedState{this};
    state.setMaterialized();
  }

  materializeInParent();
  updateJournal();
}
#else

Future<std::tuple<BufVec, bool>>
FileInode::read(size_t size, off_t off, ObjectFetchContext& context) {
  XDCHECK_GE(off, 0);
  return runWhileDataLoaded<Future<std::tuple<BufVec, bool>>>(
      LockedState{this},
      BlobCache::Interest::WantHandle,
      // This function is only called by FUSE.
      context,
      nullptr,
      [size, off, self = inodePtrFromThis()](
          LockedState&& state,
          std::shared_ptr<const Blob> blob) -> std::tuple<BufVec, bool> {
        SCOPE_SUCCESS {
          self->updateAtimeLocked(*state);
        };

        // Materialized either before or during blob load.
        if (state->tag == State::MATERIALIZED_IN_OVERLAY) {
          // TODO(xavierd): For materialized files, only return EOF when read
          // returned no bytes. This will force some FS Channel (like NFS) to
          // issue at least 2 read calls: one for reading the entire file, and
          // the second one to get the EOF bit.
          auto buf = self->getOverlayFileAccess(state)->read(*self, size, off);
          auto eof = size != 0 && buf->empty();
          return {std::move(buf), eof};
        }

        // runWhileDataLoaded() ensures that the state is either
        // MATERIALIZED_IN_OVERLAY or BLOB_NOT_LOADING
        XDCHECK_EQ(state->tag, State::BLOB_NOT_LOADING);
        XDCHECK(blob) << "blob missing after load completed";

        state->readByteRanges.add(off, off + size);
        if (state->readByteRanges.covers(0, blob->getSize())) {
          XLOG(DBG4) << "Inode " << self->getNodeId()
                     << " dropping interest for blob " << blob->getHash()
                     << " because it's been fully read.";
          state->interestHandle.reset();
          state->readByteRanges.clear();
        }

        auto buf = blob->getContents();
        folly::io::Cursor cursor(&buf);

        if (!cursor.canAdvance(off)) {
          // Seek beyond EOF.  Return an empty result.
          return {BufVec{folly::IOBuf::wrapBuffer("", 0)}, true};
        }

        cursor.skip(off);

        std::unique_ptr<folly::IOBuf> result;
        cursor.cloneAtMost(result, size);

        return {BufVec{std::move(result)}, cursor.isAtEnd()};
      });
}

size_t FileInode::writeImpl(
    LockedState& state,
    const struct iovec* iov,
    size_t numIovecs,
    off_t off) {
  XDCHECK_EQ(state->tag, State::MATERIALIZED_IN_OVERLAY);

  auto xfer = getOverlayFileAccess(state)->write(*this, iov, numIovecs, off);

  updateMtimeAndCtimeLocked(*state, getNow());

  state.unlock();

  updateJournal();

  return xfer;
}

folly::Future<size_t>
FileInode::write(BufVec&& buf, off_t off, ObjectFetchContext& fetchContext) {
  return runWhileMaterialized(
      LockedState{this},
      nullptr,
      [buf = std::move(buf), off, self = inodePtrFromThis()](
          LockedState&& state) {
        auto vec = buf->getIov();
        return self->writeImpl(state, vec.data(), vec.size(), off);
      },
      fetchContext);
}

folly::Future<size_t> FileInode::write(
    folly::StringPiece data,
    off_t off,
    ObjectFetchContext& fetchContext) {
  auto state = LockedState{this};

  // If we are currently materialized we don't need to copy the input data.
  if (state->tag == State::MATERIALIZED_IN_OVERLAY) {
    struct iovec iov;
    iov.iov_base = const_cast<char*>(data.data());
    iov.iov_len = data.size();
    return writeImpl(state, &iov, 1, off);
  }

  return runWhileMaterialized(
      std::move(state),
      nullptr,
      [data = data.str(), off, self = inodePtrFromThis()](
          LockedState&& stateLock) {
        struct iovec iov;
        iov.iov_base = const_cast<char*>(data.data());
        iov.iov_len = data.size();
        return self->writeImpl(stateLock, &iov, 1, off);
      },
      fetchContext);
}
#endif

Future<std::shared_ptr<const Blob>> FileInode::startLoadingData(
    LockedState state,
    BlobCache::Interest interest,
    ObjectFetchContext& fetchContext) {
  XDCHECK_EQ(state->tag, State::BLOB_NOT_LOADING);

  // Start the blob load first in case this throws an exception.
  // Ideally the state transition is no-except in tandem with the
  // Future's .then call.
  auto getBlobFuture = getMount()->getBlobAccess()->getBlob(
      state->nonMaterializedState->hash, fetchContext, interest);
  auto blobLoadingPromise =
      std::make_unique<folly::SharedPromise<std::shared_ptr<const Blob>>>();

  // Everything from here through blobFuture.then should be noexcept.
  state->blobLoadingPromise = std::move(blobLoadingPromise);
  auto resultFuture = state->blobLoadingPromise->getFuture();
  state->tag = State::BLOB_LOADING;

  // Unlock state_ while we wait on the blob data to load
  state.unlock();

  auto self = inodePtrFromThis(); // separate line for formatting
  std::move(getBlobFuture)
      .thenTry([self](folly::Try<BlobCache::GetResult> tryResult) mutable {
        auto state = LockedState{self};

        switch (state->tag) {
          case State::BLOB_NOT_LOADING:
            EDEN_BUG()
                << "A blob load finished when the inode was in BLOB_NOT_LOADING state";

          // Since the load doesn't hold the state lock for its duration,
          // sanity check that the inode is still in loading state.
          //
          // Note that someone else may have grabbed the lock before us and
          // materialized the FileInode, so we may already be
          // MATERIALIZED_IN_OVERLAY at this point.
          case State::BLOB_LOADING: {
            auto promise = std::move(*state->blobLoadingPromise);
            state->blobLoadingPromise.reset();
            state->tag = State::BLOB_NOT_LOADING;

            // Call the Future's subscribers while the state_ lock is not
            // held. Even if the FileInode has transitioned to a materialized
            // state, any pending loads must be unblocked.
            if (tryResult.hasValue()) {
              state->interestHandle = std::move(tryResult->interestHandle);
              state.unlock();
              promise.setValue(std::move(tryResult->object));
            } else {
              state.unlock();
              promise.setException(std::move(tryResult).exception());
            }
            return;
          }

          case State::MATERIALIZED_IN_OVERLAY:
            // The load raced with a someone materializing the file to truncate
            // it.  Nothing left to do here. The truncation completed the
            // promise with a null blob.
            XCHECK_EQ(state->blobLoadingPromise.get(), nullptr);
            return;
        }
      })
      .thenError([](folly::exception_wrapper&&) {
        // We get here if EDEN_BUG() didn't terminate the process, or if we
        // threw in the preceding block.  Both are bad because we won't
        // automatically propagate the exception to resultFuture and we
        // can't trust the state of anything if we get here.
        // Rather than leaving something hanging, we suicide.
        // We could probably do a bit better with the error handling here :-/
        XLOG(FATAL)
            << "Failed to propagate failure in getBlob(), no choice but to die";
      });
  return resultFuture;
}

#ifndef _WIN32
void FileInode::materializeNow(
    LockedState& state,
    std::shared_ptr<const Blob> blob,
    ObjectFetchContext& /*fetchContext*/) {
  // This function should only be called from the BLOB_NOT_LOADING state
  XDCHECK_EQ(state->tag, State::BLOB_NOT_LOADING);

  // The fetchContext can not be safely used here yet because we are not going
  // to wait for the metadata fetch future. We need to manage the fetch
  // context with a shared ptr before we can use it here.
  static auto context = ObjectFetchContext::getNullContextWithCauseDetail(
      "FileInode::materializeNow");
  // If the blob metadata is immediately available, use it to populate the SHA-1
  // value in the overlay for this file.
  // Since this uses state->nonMaterializedState->hash we perform this before
  // calling state.setMaterialized().
  auto blobSha1Future = getObjectStore()->getBlobSha1(
      state->nonMaterializedState->hash, *context);
  std::optional<Hash20> blobSha1;
  if (blobSha1Future.isReady()) {
    blobSha1 = std::move(blobSha1Future).get();
  }

  getOverlayFileAccess(state)->createFile(getNodeId(), *blob, blobSha1);

  state.setMaterialized();
}

void FileInode::materializeAndTruncate(LockedState& state) {
  XCHECK_NE(state->tag, State::MATERIALIZED_IN_OVERLAY);
  getOverlayFileAccess(state)->createEmptyFile(getNodeId());
  state.setMaterialized();
}

void FileInode::truncateInOverlay(LockedState& state) {
  XCHECK_EQ(state->tag, State::MATERIALIZED_IN_OVERLAY);
  XCHECK(!state->nonMaterializedState);

  getOverlayFileAccess(state)->truncate(*this);
}

OverlayFileAccess* FileInode::getOverlayFileAccess(LockedState&) const {
  return getMount()->getOverlayFileAccess();
}
#endif // !_WIN32

ObjectStore* FileInode::getObjectStore() const {
  return getMount()->getObjectStore();
}

void FileInode::logAccess(ObjectFetchContext& fetchContext) {
  auto ino = getNodeId();

  // Don't log root inode access
  if (ino == kRootNodeId) {
    return;
  }

  std::optional<std::string> fetchDetail;

  const auto& detail = fetchContext.getCauseDetail();
  if (detail.has_value()) {
    fetchDetail.emplace(detail.value().str());
  }

  getMount()->getServerState()->getRawHiveLogger()->logFileAccess(FileAccess{
      ino,
      fetchContext.getCause(),
      std::move(fetchDetail),
      getMount()->getWeakMount()});
}

} // namespace eden
} // namespace facebook
