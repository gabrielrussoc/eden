/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifdef _WIN32

#include "eden/fs/prjfs/PrjfsChannel.h"
#include <fmt/format.h>
#include <folly/logging/xlog.h>
#include "eden/fs/prjfs/PrjfsDispatcher.h"
#include "eden/fs/prjfs/PrjfsRequestContext.h"
#include "eden/fs/utils/Bug.h"
#include "eden/fs/utils/Guid.h"
#include "eden/fs/utils/ImmediateFuture.h"
#include "eden/fs/utils/NotImplemented.h"
#include "eden/fs/utils/PathFuncs.h"
#include "eden/fs/utils/StringConv.h"
#include "eden/fs/utils/WinError.h"

namespace facebook::eden {

namespace {

#define BAIL_ON_RECURSIVE_CALL(callbackData)                               \
  do {                                                                     \
    if (callbackData->TriggeringProcessId == GetCurrentProcessId()) {      \
      auto __path = RelativePath(callbackData->FilePathName);              \
      XLOG(ERR) << "Recursive EdenFS call are disallowed for: " << __path; \
      return HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED);                      \
    }                                                                      \
  } while (false)

detail::RcuLockedPtr getChannel(
    const PRJ_CALLBACK_DATA* callbackData) noexcept {
  XDCHECK(callbackData);
  auto* channel = static_cast<PrjfsChannel*>(callbackData->InstanceContext);
  XDCHECK(channel);
  return channel->getInner();
}

template <class Method, class... Args>
HRESULT runCallback(
    Method method,
    const PRJ_CALLBACK_DATA* callbackData,
    Args&&... args) noexcept {
  try {
    auto channel = getChannel(callbackData);
    if (!channel) {
      return HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER);
    }

    auto channelPtr = channel.get();
    auto context = std::make_shared<PrjfsRequestContext>(
        std::move(channel), *callbackData);
    return (channelPtr->*method)(
        std::move(context), callbackData, std::forward<Args>(args)...);
  } catch (const std::exception& ex) {
    return exceptionToHResult(ex);
  }
}

HRESULT startEnumeration(
    const PRJ_CALLBACK_DATA* callbackData,
    const GUID* enumerationId) noexcept {
  BAIL_ON_RECURSIVE_CALL(callbackData);
  return runCallback(
      &PrjfsChannelInner::startEnumeration, callbackData, enumerationId);
}

HRESULT endEnumeration(
    const PRJ_CALLBACK_DATA* callbackData,
    const GUID* enumerationId) noexcept {
  BAIL_ON_RECURSIVE_CALL(callbackData);
  return runCallback(
      &PrjfsChannelInner::endEnumeration, callbackData, enumerationId);
}

HRESULT getEnumerationData(
    const PRJ_CALLBACK_DATA* callbackData,
    const GUID* enumerationId,
    PCWSTR searchExpression,
    PRJ_DIR_ENTRY_BUFFER_HANDLE dirEntryBufferHandle) noexcept {
  BAIL_ON_RECURSIVE_CALL(callbackData);
  return runCallback(
      &PrjfsChannelInner::getEnumerationData,
      callbackData,
      enumerationId,
      searchExpression,
      dirEntryBufferHandle);
}

HRESULT getPlaceholderInfo(const PRJ_CALLBACK_DATA* callbackData) noexcept {
  BAIL_ON_RECURSIVE_CALL(callbackData);
  return runCallback(&PrjfsChannelInner::getPlaceholderInfo, callbackData);
}

HRESULT queryFileName(const PRJ_CALLBACK_DATA* callbackData) noexcept {
  BAIL_ON_RECURSIVE_CALL(callbackData);
  return runCallback(&PrjfsChannelInner::queryFileName, callbackData);
}

HRESULT getFileData(
    const PRJ_CALLBACK_DATA* callbackData,
    UINT64 byteOffset,
    UINT32 length) noexcept {
  BAIL_ON_RECURSIVE_CALL(callbackData);
  return runCallback(
      &PrjfsChannelInner::getFileData, callbackData, byteOffset, length);
}

void cancelCommand(const PRJ_CALLBACK_DATA* /*callbackData*/) noexcept {
  // TODO(T67329233): Interrupt the future.
}

HRESULT notification(
    const PRJ_CALLBACK_DATA* callbackData,
    BOOLEAN isDirectory,
    PRJ_NOTIFICATION notificationType,
    PCWSTR destinationFileName,
    PRJ_NOTIFICATION_PARAMETERS* notificationParameters) noexcept {
  BAIL_ON_RECURSIVE_CALL(callbackData);

  try {
    auto channel = getChannel(callbackData);
    if (!channel) {
      // TODO(zeyi): Something modified the working copy while it is being
      // unmounted. At this point, we have no way to deal with this properly
      // and the next time this repository is mounted, there will be a
      // discrepency between what EdenFS thinks the state of the working copy
      // should be and what it actually is. To solve this, we will need to scan
      // the working copy at mount time to find these files and fixup EdenFS
      // inodes.
      // Once the above is done, refactor this code to use runCallback.
      EDEN_BUG() << "A notification was received while unmounting";
    }

    auto channelPtr = channel.get();
    auto context = std::make_shared<PrjfsRequestContext>(
        std::move(channel), *callbackData);
    return channelPtr->notification(
        std::move(context),
        callbackData,
        isDirectory,
        notificationType,
        destinationFileName,
        notificationParameters);
  } catch (const std::exception& ex) {
    return exceptionToHResult(ex);
  }
}

} // namespace

PrjfsChannelInner::PrjfsChannelInner(
    std::unique_ptr<PrjfsDispatcher> dispatcher,
    const folly::Logger* straceLogger,
    ProcessAccessLog& processAccessLog)
    : dispatcher_(std::move(dispatcher)),
      straceLogger_(straceLogger),
      processAccessLog_(processAccessLog) {}

HRESULT PrjfsChannelInner::startEnumeration(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData,
    const GUID* enumerationId) {
  auto guid = Guid(*enumerationId);
  auto path = RelativePath(callbackData->FilePathName);

  auto fut = makeImmediateFutureWith([this,
                                      context,
                                      guid = std::move(guid),
                                      path = std::move(path)]() mutable {
    auto requestWatch =
        std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(nullptr);
    auto stat = &ChannelThreadStats::openDir;
    context->startRequest(dispatcher_->getStats(), stat, requestWatch);

    FB_LOGF(getStraceLogger(), DBG7, "opendir({}, guid={})", path, guid);
    return dispatcher_->opendir(std::move(path), *context)
        .thenValue([this, context = std::move(context), guid = std::move(guid)](
                       auto&& dirents) {
          addDirectoryEnumeration(std::move(guid), std::move(dirents));
          context->sendSuccess();
        });
  });

  context->catchErrors(std::move(fut))
      .ensure([context = std::move(context)] {})
      .semi()
      .via(&folly::QueuedImmediateExecutor::instance());

  return HRESULT_FROM_WIN32(ERROR_IO_PENDING);
}

HRESULT PrjfsChannelInner::endEnumeration(
    std::shared_ptr<PrjfsRequestContext> /* context */,
    const PRJ_CALLBACK_DATA* /*callbackData*/,
    const GUID* enumerationId) {
  auto guid = Guid(*enumerationId);
  FB_LOGF(getStraceLogger(), DBG7, "closedir({})", guid);

  removeDirectoryEnumeration(guid);

  return S_OK;
}

HRESULT PrjfsChannelInner::getEnumerationData(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData,
    const GUID* enumerationId,
    PCWSTR searchExpression,
    PRJ_DIR_ENTRY_BUFFER_HANDLE dirEntryBufferHandle) {
  auto guid = Guid(*enumerationId);

  FB_LOGF(
      getStraceLogger(),
      DBG7,
      "readdir({}, searchExpression={})",
      guid,
      searchExpression == nullptr
          ? "<nullptr>"
          : wideToMultibyteString<std::string>(searchExpression));

  auto optEnumerator = findDirectoryEnumeration(guid);
  if (!optEnumerator.has_value()) {
    XLOG(DBG5) << "Directory enumeration not found: " << guid;
    return HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER);
  }
  auto enumerator = std::move(optEnumerator).value();

  auto shouldRestart =
      bool(callbackData->Flags & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN);
  if (enumerator->isSearchExpressionEmpty() || shouldRestart) {
    if (searchExpression != nullptr) {
      enumerator->saveExpression(searchExpression);
    } else {
      enumerator->saveExpression(L"*");
    }
  }

  if (shouldRestart) {
    enumerator->restartEnumeration();
  }

  auto fut = makeImmediateFutureWith([this,
                                      context,
                                      enumerator = std::move(enumerator),
                                      buffer = dirEntryBufferHandle] {
    auto requestWatch =
        std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(nullptr);
    auto stat = &ChannelThreadStats::readDir;
    context->startRequest(dispatcher_->getStats(), stat, requestWatch);

    // TODO(xavierd): there is a potential quadratic cost to the following code
    // in the case where the buffer can only hold a single entry. The linear
    // getPendingDirEntries would thus be called for as many entries, causing
    // the quadratic complexity. In practice, ProjectedFS doesn't do this and
    // thus we can afford a bit of redundant work.
    auto pendingDirEntries = enumerator->getPendingDirEntries();
    return collectAll(std::move(pendingDirEntries))
        .thenValue([enumerator = std::move(enumerator),
                    buffer,
                    context = std::move(context)](
                       std::vector<folly::Try<PrjfsDirEntry::Ready>> entries) {
          bool added = false;
          for (auto& try_ : entries) {
            if (try_.hasException()) {
              return folly::Try<folly::Unit>{try_.exception()};
            }
            auto& entry = try_.value();

            auto fileInfo = PRJ_FILE_BASIC_INFO();
            fileInfo.IsDirectory = entry.isDir;
            fileInfo.FileSize = entry.size;

            XLOGF(
                DBG6,
                "Directory entry: {}, {}, size={}",
                fileInfo.IsDirectory ? "Dir" : "File",
                PathComponent(entry.name),
                fileInfo.FileSize);

            auto result =
                PrjFillDirEntryBuffer(entry.name.c_str(), &fileInfo, buffer);
            if (FAILED(result)) {
              if (result == HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER) &&
                  added) {
                // We are out of buffer space. This entry didn't make it. Return
                // without increment.
                break;
              } else {
                return folly::Try<folly::Unit>{makeHResultErrorExplicit(
                    result,
                    fmt::format(
                        FMT_STRING("Adding directory entry {}"),
                        PathComponent(entry.name)))};
              }
            }
            added = true;
            enumerator->advanceEnumeration();
          }

          context->sendEnumerationSuccess(buffer);
          return folly::Try{folly::unit};
        });
  });

  context->catchErrors(std::move(fut))
      .ensure([context = std::move(context)] {})
      .semi()
      .via(&folly::QueuedImmediateExecutor::instance());

  return HRESULT_FROM_WIN32(ERROR_IO_PENDING);
}

HRESULT PrjfsChannelInner::getPlaceholderInfo(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData) {
  auto path = RelativePath(callbackData->FilePathName);
  auto virtualizationContext = callbackData->NamespaceVirtualizationContext;

  auto fut = makeImmediateFutureWith([this,
                                      context,
                                      path = std::move(path),
                                      virtualizationContext]() mutable {
    auto requestWatch =
        std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(nullptr);
    auto stat = &ChannelThreadStats::lookup;
    context->startRequest(dispatcher_->getStats(), stat, requestWatch);

    FB_LOGF(getStraceLogger(), DBG7, "lookup({})", path);
    return dispatcher_->lookup(std::move(path), *context)
        .thenValue([context = std::move(context),
                    virtualizationContext = virtualizationContext](
                       std::optional<LookupResult>&& optLookupResult) {
          if (!optLookupResult) {
            context->sendError(HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND));
            return ImmediateFuture{folly::unit};
          }
          auto lookupResult = std::move(optLookupResult).value();

          PRJ_PLACEHOLDER_INFO placeholderInfo{};
          placeholderInfo.FileBasicInfo.IsDirectory = lookupResult.meta.isDir;
          placeholderInfo.FileBasicInfo.FileSize = lookupResult.meta.size;
          auto inodeName = lookupResult.meta.path.wide();

          HRESULT result = PrjWritePlaceholderInfo(
              virtualizationContext,
              inodeName.c_str(),
              &placeholderInfo,
              sizeof(placeholderInfo));

          if (FAILED(result)) {
            return makeImmediateFuture<folly::Unit>(makeHResultErrorExplicit(
                result,
                fmt::format(
                    FMT_STRING("Writing placeholder for {}"),
                    lookupResult.meta.path)));
          }

          context->sendSuccess();

          lookupResult.incFsRefcount();

          return ImmediateFuture{folly::unit};
        });
  });

  context->catchErrors(std::move(fut))
      .ensure([context = std::move(context)] {})
      .semi()
      .via(&folly::QueuedImmediateExecutor::instance());

  return HRESULT_FROM_WIN32(ERROR_IO_PENDING);
}

HRESULT PrjfsChannelInner::queryFileName(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData) {
  auto path = RelativePath(callbackData->FilePathName);

  auto fut = makeImmediateFutureWith([this,
                                      context,
                                      path = std::move(path)]() mutable {
    auto requestWatch =
        std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(nullptr);
    auto stat = &ChannelThreadStats::access;
    context->startRequest(dispatcher_->getStats(), stat, requestWatch);
    FB_LOGF(getStraceLogger(), DBG7, "access({})", path);
    return dispatcher_->access(std::move(path), *context)
        .thenValue([context = std::move(context)](bool present) {
          if (present) {
            context->sendSuccess();
          } else {
            context->sendError(HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND));
          }
        });
  });

  context->catchErrors(std::move(fut))
      .ensure([context = std::move(context)] {})
      .semi()
      .via(&folly::QueuedImmediateExecutor::instance());

  return HRESULT_FROM_WIN32(ERROR_IO_PENDING);
}

namespace {

struct PrjAlignedBufferDeleter {
  void operator()(void* buffer) noexcept {
    ::PrjFreeAlignedBuffer(buffer);
  }
};

HRESULT readMultipleFileChunks(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT namespaceVirtualizationContext,
    const Guid& dataStreamId,
    const std::string& content,
    uint64_t startOffset,
    uint64_t length,
    uint64_t chunkSize) {
  HRESULT result;
  std::unique_ptr<void, PrjAlignedBufferDeleter> writeBuffer{
      PrjAllocateAlignedBuffer(namespaceVirtualizationContext, chunkSize)};

  if (writeBuffer.get() == nullptr) {
    return E_OUTOFMEMORY;
  }

  uint64_t remainingLength = length;

  while (remainingLength > 0) {
    uint64_t copySize = std::min(remainingLength, chunkSize);

    //
    // TODO(puneetk): Once backing store has the support for chunking the file
    // contents, we can read the chunks of large files here and then write
    // them to FS.
    //
    // TODO(puneetk): Build an interface to backing store so that we can pass
    // the aligned buffer to avoid coping here.
    //
    RtlCopyMemory(writeBuffer.get(), content.data() + startOffset, copySize);

    // Write the data to the file in the local file system.
    result = PrjWriteFileData(
        namespaceVirtualizationContext,
        dataStreamId,
        writeBuffer.get(),
        startOffset,
        folly::to_narrow(copySize));

    if (FAILED(result)) {
      return result;
    }

    remainingLength -= copySize;
    startOffset += copySize;
  }

  return S_OK;
}

HRESULT readSingleFileChunk(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT namespaceVirtualizationContext,
    const Guid& dataStreamId,
    const std::string& content,
    uint64_t startOffset,
    uint64_t length) {
  return readMultipleFileChunks(
      namespaceVirtualizationContext,
      dataStreamId,
      content,
      /*startOffset=*/startOffset,
      /*length=*/length,
      /*writeLength=*/length);
}

uint64_t BlockAlignTruncate(uint64_t ptr, uint32_t alignment) {
  return ((ptr) & (0 - (static_cast<uint64_t>(alignment))));
}

constexpr uint32_t kMinChunkSize = 512 * 1024; // 512 KiB
constexpr uint32_t kMaxChunkSize = 5 * 1024 * 1024; // 5 MiB

} // namespace

HRESULT PrjfsChannelInner::getFileData(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData,
    UINT64 byteOffset,
    UINT32 length) {
  auto fut = makeImmediateFutureWith(
      [this,
       context,
       path = RelativePath(callbackData->FilePathName),
       virtualizationContext = callbackData->NamespaceVirtualizationContext,
       dataStreamId = Guid(callbackData->DataStreamId),
       byteOffset,
       length]() mutable {
        auto requestWatch =
            std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(
                nullptr);
        auto stat = &ChannelThreadStats::read;
        context->startRequest(dispatcher_->getStats(), stat, requestWatch);

        FB_LOGF(
            getStraceLogger(),
            DBG7,
            "read({}, off={}, len={})",
            path,
            byteOffset,
            length);
        return dispatcher_->read(std::move(path), *context)
            .thenValue([context = std::move(context),
                        virtualizationContext = virtualizationContext,
                        dataStreamId = std::move(dataStreamId),
                        byteOffset = byteOffset,
                        length = length](const std::string content) {
              //
              // We should return file data which is smaller than
              // our kMaxChunkSize and meets the memory alignment
              // requirements of the virtualization instance's storage
              // device.
              //

              HRESULT result;
              if (content.length() <= kMinChunkSize) {
                //
                // If the file is small - copy the whole file in one shot.
                //
                result = readSingleFileChunk(
                    virtualizationContext,
                    dataStreamId,
                    content,
                    /*startOffset=*/0,
                    /*writeLength=*/content.length());

              } else if (length <= kMaxChunkSize) {
                //
                // If the request is with in our kMaxChunkSize - copy the
                // entire request.
                //
                result = readSingleFileChunk(
                    virtualizationContext,
                    dataStreamId,
                    content,
                    /*startOffset=*/byteOffset,
                    /*writeLength=*/length);
              } else {
                //
                // When the request is larger than kMaxChunkSize we split
                // the request into multiple chunks.
                //
                PRJ_VIRTUALIZATION_INSTANCE_INFO instanceInfo;
                result = PrjGetVirtualizationInstanceInfo(
                    virtualizationContext, &instanceInfo);

                if (SUCCEEDED(result)) {
                  uint64_t startOffset = byteOffset;
                  uint64_t endOffset = BlockAlignTruncate(
                      startOffset + kMaxChunkSize, instanceInfo.WriteAlignment);
                  XDCHECK_GT(endOffset, 0ul);
                  XDCHECK_GT(endOffset, startOffset);

                  uint64_t chunkSize = endOffset - startOffset;
                  result = readMultipleFileChunks(
                      virtualizationContext,
                      dataStreamId,
                      content,
                      /*startOffset=*/startOffset,
                      /*length=*/length,
                      /*chunkSize=*/chunkSize);
                }
              }

              if (FAILED(result)) {
                context->sendError(result);
              } else {
                context->sendSuccess();
              }
            });
      });

  context->catchErrors(std::move(fut))
      .ensure([context = std::move(context)] {})
      .semi()
      .via(&folly::QueuedImmediateExecutor::instance());

  return HRESULT_FROM_WIN32(ERROR_IO_PENDING);
}

namespace {
typedef ImmediateFuture<folly::Unit> (PrjfsChannelInner::*NotificationHandler)(
    RelativePath oldPath,
    RelativePath destPath,
    bool isDirectory,
    ObjectFetchContext& context);

typedef std::string (*NotificationArgRenderer)(
    RelativePathPiece relPath,
    RelativePathPiece destPath,
    bool isDirectory);

struct NotificationHandlerEntry {
  constexpr NotificationHandlerEntry(
      NotificationHandler h,
      NotificationArgRenderer r,
      ChannelThreadStats::StatPtr s)
      : handler{h}, renderer{r}, stat{s} {}

  NotificationHandler handler;
  NotificationArgRenderer renderer;
  ChannelThreadStats::StatPtr stat;
};

std::string newFileCreatedRenderer(
    RelativePathPiece relPath,
    RelativePathPiece /*destPath*/,
    bool isDirectory) {
  return fmt::format(
      FMT_STRING("{}Created({})"), isDirectory ? "dir" : "file", relPath);
}

std::string fileOverwrittenRenderer(
    RelativePathPiece relPath,
    RelativePathPiece /*destPath*/,
    bool /*isDirectory*/) {
  return fmt::format(FMT_STRING("fileOverwritten({})"), relPath);
}

std::string fileHandleClosedFileModifiedRenderer(
    RelativePathPiece relPath,
    RelativePathPiece /*destPath*/,
    bool /*isDirectory*/) {
  return fmt::format(FMT_STRING("fileModified({})"), relPath);
}

std::string fileRenamedRenderer(
    RelativePathPiece oldPath,
    RelativePathPiece newPath,
    bool /*isDirectory*/) {
  return fmt::format(FMT_STRING("fileRenamed({} -> {})"), oldPath, newPath);
}

std::string preRenameRenderer(
    RelativePathPiece oldPath,
    RelativePathPiece newPath,
    bool /*isDirectory*/) {
  return fmt::format(FMT_STRING("preRename({} -> {})"), oldPath, newPath);
}

std::string fileHandleClosedFileDeletedRenderer(
    RelativePathPiece relPath,
    RelativePathPiece /*destPath*/,
    bool isDirectory) {
  return fmt::format(
      FMT_STRING("{}Deleted({})"), isDirectory ? "dir" : "file", relPath);
}

std::string preSetHardlinkRenderer(
    RelativePathPiece oldPath,
    RelativePathPiece newPath,
    bool /*isDirectory*/) {
  return fmt::format(FMT_STRING("link({} -> {})"), oldPath, newPath);
}

const std::unordered_map<PRJ_NOTIFICATION, NotificationHandlerEntry>
    notificationHandlerMap = {
        {
            PRJ_NOTIFICATION_NEW_FILE_CREATED,
            {&PrjfsChannelInner::newFileCreated,
             newFileCreatedRenderer,
             &ChannelThreadStats::newFileCreated},
        },
        {
            PRJ_NOTIFICATION_FILE_OVERWRITTEN,
            {&PrjfsChannelInner::fileOverwritten,
             fileOverwrittenRenderer,
             &ChannelThreadStats::fileOverwritten},
        },
        {
            PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED,
            {&PrjfsChannelInner::fileHandleClosedFileModified,
             fileHandleClosedFileModifiedRenderer,
             &ChannelThreadStats::fileHandleClosedFileModified},
        },
        {
            PRJ_NOTIFICATION_FILE_RENAMED,
            {&PrjfsChannelInner::fileRenamed,
             fileRenamedRenderer,
             &ChannelThreadStats::fileRenamed},
        },
        {
            PRJ_NOTIFICATION_PRE_RENAME,
            {&PrjfsChannelInner::preRename,
             preRenameRenderer,
             &ChannelThreadStats::preRenamed},
        },
        {
            PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED,
            {&PrjfsChannelInner::fileHandleClosedFileDeleted,
             fileHandleClosedFileDeletedRenderer,
             &ChannelThreadStats::fileHandleClosedFileDeleted},
        },
        {
            PRJ_NOTIFICATION_PRE_SET_HARDLINK,
            {&PrjfsChannelInner::preSetHardlink,
             preSetHardlinkRenderer,
             &ChannelThreadStats::preSetHardlink},
        },
};
} // namespace

ImmediateFuture<folly::Unit> PrjfsChannelInner::newFileCreated(
    RelativePath relPath,
    RelativePath /*destPath*/,
    bool isDirectory,
    ObjectFetchContext& context) {
  if (isDirectory) {
    return dispatcher_->dirCreated(std::move(relPath), context);
  } else {
    return dispatcher_->fileCreated(std::move(relPath), context);
  }
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::fileOverwritten(
    RelativePath relPath,
    RelativePath /*destPath*/,
    bool /*isDirectory*/,
    ObjectFetchContext& context) {
  return dispatcher_->fileModified(std::move(relPath), context);
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::fileHandleClosedFileModified(
    RelativePath relPath,
    RelativePath /*destPath*/,
    bool /*isDirectory*/,
    ObjectFetchContext& context) {
  return dispatcher_->fileModified(std::move(relPath), context);
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::fileRenamed(
    RelativePath oldPath,
    RelativePath newPath,
    bool isDirectory,
    ObjectFetchContext& context) {
  // When files are moved in and out of the repo, the rename paths are
  // empty, handle these like creation/removal of files.
  if (oldPath.empty()) {
    return newFileCreated(
        std::move(newPath), RelativePath{}, isDirectory, context);
  } else if (newPath.empty()) {
    return fileHandleClosedFileDeleted(
        std::move(oldPath), RelativePath{}, isDirectory, context);
  } else {
    return dispatcher_->fileRenamed(
        std::move(oldPath), std::move(newPath), context);
  }
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::preRename(
    RelativePath /*oldPath*/,
    RelativePath /*newPath*/,
    bool /*isDirectory*/,
    ObjectFetchContext& /*context*/) {
  return folly::unit;
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::fileHandleClosedFileDeleted(
    RelativePath oldPath,
    RelativePath /*destPath*/,
    bool isDirectory,
    ObjectFetchContext& context) {
  if (isDirectory) {
    return dispatcher_->dirDeleted(std::move(oldPath), context);
  } else {
    return dispatcher_->fileDeleted(std::move(oldPath), context);
  }
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::preSetHardlink(
    RelativePath relPath,
    RelativePath /*newPath*/,
    bool /*isDirectory*/,
    ObjectFetchContext& context) {
  return folly::Try<folly::Unit>(makeHResultErrorExplicit(
      HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED),
      fmt::format(FMT_STRING("Hardlinks are not supported: {}"), relPath)));
}

HRESULT PrjfsChannelInner::notification(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData,
    BOOLEAN isDirectory,
    PRJ_NOTIFICATION notificationType,
    PCWSTR destinationFileName,
    PRJ_NOTIFICATION_PARAMETERS* /*notificationParameters*/) {
  auto it = notificationHandlerMap.find(notificationType);
  if (it == notificationHandlerMap.end()) {
    XLOG(WARN) << "Unrecognized notification: " << notificationType;
    return HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER);
  } else {
    auto stat = it->second.stat;
    auto handler = it->second.handler;
    auto renderer = it->second.renderer;

    auto relPath = RelativePath(callbackData->FilePathName);
    auto destPath = RelativePath(destinationFileName);

    auto fut = makeImmediateFutureWith([this,
                                        context,
                                        stat = stat,
                                        handler = handler,
                                        renderer = renderer,
                                        relPath = std::move(relPath),
                                        destPath = std::move(destPath),
                                        isDirectory]() mutable {
      auto requestWatch =
          std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(nullptr);
      context->startRequest(dispatcher_->getStats(), stat, requestWatch);

      FB_LOG(getStraceLogger(), DBG7, renderer(relPath, destPath, isDirectory));
      return (this->*handler)(
                 std::move(relPath), std::move(destPath), isDirectory, *context)
          .thenValue([context = std::move(context)](auto&&) {
            context->sendNotificationSuccess();
          });
    });

    context->catchErrors(std::move(fut))
        .ensure([context = std::move(context)] {})
        .semi()
        .via(&folly::QueuedImmediateExecutor::instance());

    return HRESULT_FROM_WIN32(ERROR_IO_PENDING);
  }
}

namespace {
void sendReply(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT context,
    int32_t commandId,
    HRESULT result,
    PRJ_COMPLETE_COMMAND_EXTENDED_PARAMETERS* FOLLY_NULLABLE extra) {
  result = PrjCompleteCommand(context, commandId, result, extra);
  if (FAILED(result)) {
    XLOG(ERR) << "Couldn't complete command: " << commandId << ": "
              << win32ErrorToString(result);
  }
}
} // namespace

void PrjfsChannelInner::sendSuccess(
    int32_t commandId,
    PRJ_COMPLETE_COMMAND_EXTENDED_PARAMETERS* FOLLY_NULLABLE extra) {
  sendReply(mountChannel_, commandId, S_OK, extra);
}

void PrjfsChannelInner::sendError(int32_t commandId, HRESULT result) {
  sendReply(mountChannel_, commandId, result, nullptr);
}

namespace {
folly::Indestructible<folly::rcu_domain<detail::RcuTag>> prjfsRcuDomain;
}

PrjfsChannel::PrjfsChannel(
    AbsolutePathPiece mountPath,
    std::unique_ptr<PrjfsDispatcher> dispatcher,
    const folly::Logger* straceLogger,
    std::shared_ptr<ProcessNameCache> processNameCache,
    Guid guid)
    : mountPath_(mountPath),
      mountId_(std::move(guid)),
      processAccessLog_(std::move(processNameCache)),
      inner_(
          *prjfsRcuDomain,
          std::move(dispatcher),
          straceLogger,
          processAccessLog_) {}

PrjfsChannel::~PrjfsChannel() {
  XCHECK(stopPromise_.isFulfilled())
      << "stop() must be called before destroying the channel";
}

void PrjfsChannel::start(bool readOnly, bool useNegativePathCaching) {
  if (readOnly) {
    NOT_IMPLEMENTED();
  }

  auto callbacks = PRJ_CALLBACKS();
  callbacks.StartDirectoryEnumerationCallback = startEnumeration;
  callbacks.EndDirectoryEnumerationCallback = endEnumeration;
  callbacks.GetDirectoryEnumerationCallback = getEnumerationData;
  callbacks.GetPlaceholderInfoCallback = getPlaceholderInfo;
  callbacks.GetFileDataCallback = getFileData;
  callbacks.NotificationCallback = notification;
  callbacks.QueryFileNameCallback = queryFileName;
  callbacks.CancelCommandCallback = cancelCommand;

  PRJ_NOTIFICATION_MAPPING notificationMappings[] = {
      {PRJ_NOTIFY_NEW_FILE_CREATED | PRJ_NOTIFY_FILE_OVERWRITTEN |
           PRJ_NOTIFY_PRE_RENAME | PRJ_NOTIFY_FILE_RENAMED |
           PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED |
           PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED |
           PRJ_NOTIFY_PRE_SET_HARDLINK,
       L""},
  };

  auto startOpts = PRJ_STARTVIRTUALIZING_OPTIONS();
  startOpts.NotificationMappings = notificationMappings;
  startOpts.NotificationMappingsCount =
      folly::to_narrow(std::size(notificationMappings));

  useNegativePathCaching_ = useNegativePathCaching;
  if (useNegativePathCaching) {
    startOpts.Flags = PRJ_FLAG_USE_NEGATIVE_PATH_CACHE;
  }

  XLOGF(
      INFO,
      "Starting PrjfsChannel for: {} with GUID: {}",
      mountPath_,
      mountId_);

  auto winPath = mountPath_.wide();

  auto result = PrjMarkDirectoryAsPlaceholder(
      winPath.c_str(), nullptr, nullptr, mountId_);

  if (FAILED(result) &&
      result != HRESULT_FROM_WIN32(ERROR_REPARSE_POINT_ENCOUNTERED)) {
    throw makeHResultErrorExplicit(
        result,
        fmt::format(
            FMT_STRING("Failed to setup the mount point: {}"), mountPath_));
  }

  result = PrjStartVirtualizing(
      winPath.c_str(), &callbacks, this, &startOpts, &mountChannel_);

  if (FAILED(result)) {
    throw makeHResultErrorExplicit(result, "Failed to start the mount point");
  }

  // On Windows, negative path cache is kept between channels. Invalidating here
  // gives our user an easy way to get out of a situation where an incorrect
  // negative path result is cached by Windows without rebooting.
  flushNegativePathCache();

  inner_.rlock()->setMountChannel(mountChannel_);

  XLOG(INFO) << "Started PrjfsChannel for: " << mountPath_;
}

folly::SemiFuture<folly::Unit> PrjfsChannel::stop() {
  XLOG(INFO) << "Stopping PrjfsChannel for: " << mountPath_;
  XCHECK(!stopPromise_.isFulfilled());
  PrjStopVirtualizing(mountChannel_);
  mountChannel_ = nullptr;

  return folly::makeSemiFutureWith([this] {
    inner_.reset();
    inner_.synchronize();
    stopPromise_.setValue(StopData{});
  });
}

folly::SemiFuture<PrjfsChannel::StopData> PrjfsChannel::getStopFuture() {
  return stopPromise_.getSemiFuture();
}

// TODO: We need to add an extra layer to absorb all the exceptions generated in
// Eden from leaking into FS. This would come in soon.

folly::Try<void> PrjfsChannel::removeCachedFile(RelativePathPiece path) {
  if (path.empty()) {
    return folly::Try<void>{};
  }

  auto winPath = path.wide();

  XLOG(DBG6) << "Invalidating: " << path;

  PRJ_UPDATE_FAILURE_CAUSES failureReason;
  auto result = PrjDeleteFile(
      mountChannel_,
      winPath.c_str(),
      PRJ_UPDATE_ALLOW_DIRTY_METADATA | PRJ_UPDATE_ALLOW_DIRTY_DATA |
          PRJ_UPDATE_ALLOW_READ_ONLY | PRJ_UPDATE_ALLOW_TOMBSTONE,
      &failureReason);
  if (FAILED(result)) {
    if (result == HRESULT_FROM_WIN32(ERROR_REPARSE_POINT_ENCOUNTERED)) {
      // We've attempted to call PrjDeleteFile on a directory. That isn't
      // supported, let's just ignore.
    } else if (
        result == HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND) ||
        result == HRESULT_FROM_WIN32(ERROR_PATH_NOT_FOUND)) {
      // The file or a directory in the path is not cached, ignore.
    } else {
      return folly::Try<void>{makeHResultErrorExplicit(
          result,
          fmt::format(
              FMT_STRING("Couldn't delete file {}: {:#x}"),
              path,
              static_cast<uint32_t>(result)))};
    }
  }

  return folly::Try<void>{};
}

folly::Try<void> PrjfsChannel::addDirectoryPlaceholder(RelativePathPiece path) {
  if (path.empty()) {
    return folly::Try<void>{};
  }

  auto winMountPath = mountPath_.wide();
  auto fullPath = mountPath_ + path;
  auto winPath = fullPath.wide();

  XLOGF(DBG6, "Adding a placeholder for: {}", path);
  auto result = PrjMarkDirectoryAsPlaceholder(
      winMountPath.c_str(), winPath.c_str(), nullptr, mountId_);
  if (FAILED(result)) {
    if (result == HRESULT_FROM_WIN32(ERROR_REPARSE_POINT_ENCOUNTERED)) {
      // This is already a placeholder, not an error.
    } else if (result == HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED)) {
      // TODO(T78476916): The access denied are coming from
      // PrjMarkDirectoryAsPlaceholder recursively calling into EdenFS, which
      // is denied by the BAIL_ON_RECURSIVE_CALL macro.
      //
      // In theory this means that EdenFS is invalidating a directory that
      // isn't materialized, ie: doing useless work. Despite having a negative
      // performance impact, this doesn't affect correctness, so ignore for now.
      //
      // A long term fix will need to not issue invalidation on directories
      // that aren't materialized.
      XLOG_EVERY_MS(WARN, 100) << fmt::format(
          FMT_STRING(
              "Couldn't add a placeholder for: {}, as it triggered a recursive EdenFS call"),
          path);
    } else {
      return folly::Try<void>{makeHResultErrorExplicit(
          result,
          fmt::format(
              FMT_STRING("Couldn't add a placeholder for {}: {:#x}"),
              path,
              static_cast<uint32_t>(result)))};
    }
  }

  return folly::Try<void>{};
}

void PrjfsChannel::flushNegativePathCache() {
  if (useNegativePathCaching_) {
    XLOG(DBG6) << "Flushing negative path cache";

    uint32_t numFlushed = 0;
    auto result = PrjClearNegativePathCache(mountChannel_, &numFlushed);
    if (FAILED(result)) {
      throwHResultErrorExplicit(
          result, "Couldn't flush the negative path cache");
    }

    XLOGF(DBG6, "Flushed {} entries", numFlushed);
  }
}

} // namespace facebook::eden

#endif
